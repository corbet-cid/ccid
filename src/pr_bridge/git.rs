use super::{http, Config, Objects};
use crate::{failure, Result, Runner};
use std::{
    fs,
    io::Write,
    time::{Duration, Instant},
};

pub(super) struct Git {
    root: tempfile::TempDir,
    askpass: tempfile::TempPath,
    forgejo_user: String,
    deadline: Instant,
    #[cfg(test)]
    pub(super) local_urls: Option<(String, String)>,
}

impl Git {
    pub fn new(timeout: Duration) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let mut askpass = tempfile::NamedTempFile::new()?;
        askpass.write_all(b"#!/bin/sh\ncase \"$1\" in *Username*) printf '%s\\n' \"$CCID_BRIDGE_GIT_USER\";; *Password*) printf '%s\\n' \"$CCID_BRIDGE_GIT_TOKEN\";; *) exit 1;; esac\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(askpass.path(), fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            root,
            askpass: askpass.into_temp_path(),
            forgejo_user: String::new(),
            deadline: Instant::now() + timeout,
            #[cfg(test)]
            local_urls: None,
        })
    }

    fn run(&self, args: &[&str], credential: Option<&str>) -> Result<String> {
        let mut environment = http::environment();
        for (name, value) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_LFS_SKIP_SMUDGE", "1"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
            ("GIT_ATTR_NOSYSTEM", "1"),
            ("LC_ALL", "C"),
        ] {
            environment.insert(name.into(), value.into());
        }
        if let Some(name) = credential {
            environment.insert("CCID_BRIDGE_GIT_TOKEN".into(), http::token(name)?.into());
            environment.insert("GIT_ASKPASS".into(), self.askpass.as_os_str().to_owned());
            environment.insert(
                "CCID_BRIDGE_GIT_USER".into(),
                if name == "CCID_BRIDGE_GITLAB_TOKEN" {
                    "oauth2"
                } else {
                    &self.forgejo_user
                }
                .into(),
            );
        }
        let runner = Runner::until(self.root.path().to_owned(), environment, self.deadline)?
            .with_stderr_events()
            .without_child_stderr();
        let mut command: Vec<String> = [
            "git",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "http.followRedirects=false",
            "-c",
            "http.sslVerify=true",
            "-c",
            "fetch.fsckObjects=true",
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "gc.auto=0",
            "-c",
            "push.followTags=false",
            "-c",
            "push.recurseSubmodules=no",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        #[cfg(test)]
        if self.local_urls.is_some() {
            command.extend(["-c".into(), "protocol.file.allow=always".into()]);
        }
        command.extend(args.iter().map(|s| (*s).into()));
        runner.run(&command, true)
    }
}

impl Objects for Git {
    fn publish(&mut self, config: &Config, iid: u64, sha: &str, branch: &str) -> Result<()> {
        self.forgejo_user = config.import_repository.split('/').next().unwrap().into();
        let source = format!("{}/{}.git", config.gitlab, config.gitlab_repository);
        let destination = format!("{}/{}.git", config.forgejo, config.import_repository);
        let (source, destination) = (source.as_str(), destination.as_str());
        let (read_token, write_token) = (
            Some("CCID_BRIDGE_GITLAB_TOKEN"),
            Some("CCID_BRIDGE_FORGEJO_TOKEN"),
        );
        #[cfg(test)]
        let (source, destination, read_token, write_token) = match &self.local_urls {
            Some((s, d)) => (s.as_str(), d.as_str(), None, None),
            None => (source, destination, read_token, write_token),
        };
        self.run(&["init", "--bare", "--template=", "."], None)?;
        let remote_ref = format!("refs/heads/{branch}");
        let inventory = self.run(
            &["ls-remote", "--refs", "--", destination, &remote_ref],
            write_token,
        )?;
        let previous = if inventory.is_empty() {
            None
        } else {
            let (value, reference) = inventory
                .split_once('\t')
                .ok_or_else(|| failure("Malformed import ref inventory"))?;
            if !super::oid(value) || reference != remote_ref {
                return Err(failure("Unexpected import ref inventory"));
            }
            Some(value)
        };
        if previous == Some(sha) {
            return Ok(());
        }
        self.run(
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                source,
                &format!("refs/merge-requests/{iid}/head:refs/bridge/source"),
            ],
            read_token,
        )?;
        if self.run(
            &["rev-parse", "--verify", "refs/bridge/source^{commit}"],
            None,
        )? != sha
        {
            return Err(failure(
                "Fetched MR head differs from the GitLab API observation",
            ));
        }
        if self.run(&["rev-parse", "--is-shallow-repository"], None)? != "false" {
            return Err(failure("Bridge requires complete commit ancestry"));
        }
        if let Some(previous) = previous {
            self.run(
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "--",
                    destination,
                    &format!("{remote_ref}:refs/bridge/previous"),
                ],
                write_token,
            )?;
            if self.run(&["rev-parse", "refs/bridge/previous"], None)? != previous {
                return Err(failure("Import branch changed concurrently"));
            }
            self.run(&["merge-base", "--is-ancestor", previous, sha], None)
                .map_err(|_| {
                    failure("MR was rebased or import branch diverged; no force push performed")
                })?;
        }
        // Git's ordinary non-forced update also checks ancestry at the receiver.
        self.run(
            &[
                "push",
                "--quiet",
                "--no-verify",
                "--",
                destination,
                &format!("{sha}:{remote_ref}"),
            ],
            write_token,
        )?;
        let published = self.run(
            &["ls-remote", "--refs", "--", destination, &remote_ref],
            write_token,
        )?;
        if published != format!("{sha}\t{remote_ref}") {
            return Err(failure(
                "Published import head not confirmed; reconcile before continuing",
            ));
        }
        Ok(())
    }
}
