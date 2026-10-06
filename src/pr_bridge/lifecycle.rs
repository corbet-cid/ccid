use super::*;

pub(super) fn recorded_pull(
    api: &mut impl Api,
    config: &Config,
    iid: u64,
    journal: &Journal,
) -> Result<(String, Entry, Value)> {
    let root = journal.load(&config.key(iid))?;
    let key = generation_key(config, iid, root.generation.as_deref());
    let entry = journal.load(&key)?;
    let index = entry
        .pull_number
        .ok_or_else(|| failure("No journaled import for this MR"))?;
    let dest = preflight(api, config)?;
    let pull = get(
        api,
        Forge::Forgejo,
        &format!("repos/{}/pulls/{index}", config.primary_repository),
    )?;
    let marker = format!("<!-- ccid-pr-bridge:v1:{key} -->\n");
    let branch = format!("ccid-import/gitlab/{key}");
    match_pull(std::slice::from_ref(&pull), &marker, &branch, config, &dest)?
        .ok_or_else(|| failure("Journaled PR identity changed"))?;
    if number(&pull, "number")? != index {
        return Err(failure("Unexpected primary PR number"));
    }
    Ok((key, entry, pull))
}

pub(super) fn feedback(
    api: &mut impl Api,
    config: &Config,
    iid: u64,
    journal: &Journal,
) -> Result<Value> {
    let (key, mut entry, pull) = recorded_pull(api, config, iid, journal)?;
    let index = number(&pull, "number")?;
    if pull["state"] != "closed" {
        return Err(failure("Primary PR is not closed or merged"));
    }
    let path = format!("projects/{}/merge_requests/{iid}", config.gitlab_project);
    let source = get(api, Forge::Gitlab, &path)?;
    let mut snapshot: MergeRequest = serde_json::from_value(source.clone())?;
    // Validate identity even after a previous close succeeded with a lost response.
    snapshot.state = "opened".into();
    snapshot.validate(config, iid)?;
    if entry.sha.as_deref() != Some(snapshot.sha.as_str()) {
        return Err(failure(
            "Source head advanced after import; refuse to close new work",
        ));
    }
    if !matches!(source["state"].as_str(), Some("opened" | "closed")) {
        return Err(failure("Source MR has an unexpected disposition"));
    }
    let actor = number(&get(api, Forge::Gitlab, "user")?, "id")?;
    let marker = format!("<!-- ccid-pr-bridge:feedback:{key} -->");
    let url = format!(
        "{}/{}/pulls/{index}",
        config.forgejo, config.primary_repository
    );
    let disposition = if pull["merged"] == true {
        "merged"
    } else if pull["merged"] == false {
        "closed"
    } else {
        return Err(failure("Unknown primary merge state"));
    };
    let body = format!("{marker}\nThe primary contribution was {disposition}: {url}\nClosing this secondary MR; the primary forge retains the review and merge history.");
    let notes = pages(api, Forge::Gitlab, &format!("{path}/notes"))?;
    let matching: Vec<_> = notes
        .iter()
        .filter(|n| n["body"].as_str().is_some_and(|b| b.starts_with(&marker)))
        .collect();
    if matching.len() > 1
        || matching
            .iter()
            .any(|n| n["author"]["id"] != actor || n["body"] != body)
    {
        return Err(failure("Ambiguous or altered source feedback"));
    }
    if let Some(note) = matching.first() {
        let id = number(note, "id")?;
        if entry.feedback_note.is_some_and(|old| old != id) {
            return Err(failure("Source feedback identity changed"));
        }
        entry.feedback_note = Some(id);
    } else {
        if entry.feedback_attempted || entry.feedback_note.is_some() {
            return Err(failure(
                "Unresolved feedback intent; refusing duplicate comment",
            ));
        }
        entry.feedback_attempted = true;
        journal.save(&key, &entry)?;
        let note = api.request(
            Forge::Gitlab,
            "POST",
            &format!("{path}/notes"),
            Some(&json!({"body":body})),
        )?;
        if note["body"] != body || note["author"]["id"] != actor {
            return Err(failure("Feedback comment identity not confirmed"));
        }
        entry.feedback_note = Some(number(&note, "id")?);
    }
    journal.save(&key, &entry)?;
    // Re-read before the idempotent close. Never call GitLab's merge endpoint.
    let latest = get(api, Forge::Gitlab, &path)?;
    if latest["sha"] != source["sha"] || latest["target_branch"] != source["target_branch"] {
        return Err(failure("Source changed before closure"));
    }
    if latest["state"] == "opened" {
        api.request(
            Forge::Gitlab,
            "PUT",
            &path,
            Some(&json!({"state_event":"close"})),
        )?;
    } else if latest["state"] != "closed" {
        return Err(failure("Source disposition changed before closure"));
    }
    let closed = get(api, Forge::Gitlab, &path)?;
    if closed["state"] != "closed" || closed["sha"] != source["sha"] {
        return Err(failure("Source closure not confirmed"));
    }
    Ok(
        json!({"status":"source_closed","source":config.source_url(iid),"primary":url,"disposition":disposition,"note":entry.feedback_note}),
    )
}

pub(super) fn replace(
    api: &mut impl Api,
    objects: &mut impl Objects,
    config: &Config,
    iid: u64,
    journal: &Journal,
    head: &str,
) -> Result<Value> {
    if !oid(head) {
        return Err(failure("Replacement requires an exact head SHA"));
    }
    let source = mr(api, config, iid)?;
    if source.sha != head {
        return Err(failure("Replacement head differs from source"));
    }
    let root_key = config.key(iid);
    let mut root = journal.load(&root_key)?;
    let old_index = if root.generation.as_deref() == Some(head) {
        root.superseded_pull
            .ok_or_else(|| failure("Missing replacement predecessor"))?
    } else {
        let (_, entry, pull) = recorded_pull(api, config, iid, journal)?;
        if pull["state"] != "open" || pull["merged"] != false || entry.sha.as_deref() == Some(head)
        {
            return Err(failure(
                "Replacement requires an open PR and a changed source head",
            ));
        }
        number(&pull, "number")?
    };
    // New immutable generation: no force push, no history rewrite, no reused approvals.
    let result = import_generation(api, objects, config, iid, journal, Some(head))?;
    let index = number(&result, "pull_number")?;
    root.generation = Some(head.into());
    root.superseded_pull = Some(old_index);
    journal.save(&root_key, &root)?;
    let old_path = format!("repos/{}/pulls/{old_index}", config.primary_repository);
    let old = get(api, Forge::Forgejo, &old_path)?;
    if old["merged"] != false {
        return Err(failure(
            "Predecessor merged during replacement; review manually",
        ));
    }
    let backlink = format!("\n\nSuperseded after source head replacement by {}/{}/pulls/{index}. Previous reviews do not approve the replacement head.",config.forgejo,config.primary_repository);
    let body = old["body"]
        .as_str()
        .ok_or_else(|| failure("Missing predecessor body"))?;
    if old["state"] == "open" {
        api.request(
            Forge::Forgejo,
            "PATCH",
            &old_path,
            Some(&json!({"state":"closed","body":format!("{body}{backlink}")})),
        )?;
    }
    let closed = get(api, Forge::Forgejo, &old_path)?;
    if closed["state"] != "closed"
        || !closed["body"]
            .as_str()
            .is_some_and(|b| b.ends_with(&backlink))
    {
        return Err(failure("Replacement predecessor closure not confirmed"));
    }
    Ok(
        json!({"status":"replaced","superseded_pull":old_index,"pull_number":index,"head":head,"ci":"approval_required"}),
    )
}
