"""Execute the original bootstrap workflows' shell environment preparation."""
import json
import os
from pathlib import Path
import subprocess
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
URL = "https://github.com/corbet-libs/ccid"


class BootstrapIdentityTests(unittest.TestCase):
    def environment(self, workflow, repository):
        source = (ROOT / ".crow" / (workflow + ".yaml")).read_text()
        prefix = source.split("      - |\n", 1)[1].split("        exec python3", 1)[0]
        command = textwrap.dedent(prefix).replace("$$", "$")
        command += "python3 -c 'import json, os; print(json.dumps(os.environ[\"CI_REPOSITORY_URL\"]))'\n"
        env = dict(os.environ)
        env.pop("CI_REPOSITORY_URL", None)
        if repository is not None:
            env["CI_REPOSITORY_URL"] = repository
        result = subprocess.run(["bash", "-c", command], env=env, text=True,
                                capture_output=True, check=True)
        return json.loads(result.stdout)

    def test_original_build_and_test_supply_identity_without_pinned_tool(self):
        for workflow in ("build", "test"):
            for absent in (None, ""):
                with self.subTest(workflow=workflow, repository=absent):
                    self.assertEqual(self.environment(workflow, absent), URL)

    def test_explicit_identity_reaches_source_binding_unchanged(self):
        for workflow in ("build", "test"):
            for repository in (URL, "https://github.com/example/foreign", "malformed"):
                with self.subTest(workflow=workflow, repository=repository):
                    self.assertEqual(self.environment(workflow, repository), repository)


if __name__ == "__main__":
    unittest.main()
