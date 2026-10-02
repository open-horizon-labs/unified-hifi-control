"""Execute shared setup shell steps and model GitHub's next-step PATH update."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class CargoPathContract(unittest.TestCase):
    def test_isolated_cargo_tools_are_available_in_the_next_step(self):
        repository = Path(__file__).resolve().parents[1]
        source = (repository / '.github/actions/setup-rust/action.yml').read_text()
        shell_blocks = []
        lines = source.splitlines()
        for i, line in enumerate(lines):
            if line == '      run: |':
                block = []
                for body in lines[i + 1:]:
                    if not body.startswith('        '):
                        break
                    block.append(body[8:])
                shell_blocks.append('\n'.join(block))
        with tempfile.TemporaryDirectory(prefix='uhc cargo path ') as temporary:
            directory = Path(temporary)
            cargo_directory = directory / 'GitHub Actions 1000027953' / 'cargo'
            tool_directory = cargo_directory / 'bin'
            tool_directory.mkdir(parents=True)
            for tool in ['dx', 'cargo-zigbuild']:
                executable = tool_directory / tool
                executable.write_text('#!/bin/sh\nprintf "%s\\n" isolated-cargo-tool\n')
                executable.chmod(0o755)
            path_file = directory / 'github-path'
            path_file.touch()
            environment = dict(os.environ, CARGO_HOME=str(cargo_directory),
                               GITHUB_PATH=str(path_file),
                               GITHUB_OUTPUT=str(directory / 'github-output'),
                               GITHUB_ENV=str(directory / 'github-env'), PATH='/usr/bin:/bin')
            for block in shell_blocks:
                subprocess.run(['/bin/bash', '-e', '-c', block], cwd=repository,
                               env=environment, check=True)
            registered = path_file.read_text().splitlines()
            self.assertIn(str(tool_directory), registered,
                          'shared setup must register the isolated Cargo bin for subsequent steps')
            next_environment = dict(environment, PATH=os.pathsep.join(registered + [environment['PATH']]))
            for tool in ['dx', 'cargo-zigbuild']:
                result = subprocess.run(['/bin/bash', '-e', '-c', tool], env=next_environment,
                                        check=True, text=True, capture_output=True)
                self.assertEqual(result.stdout.strip(), 'isolated-cargo-tool')


if __name__ == '__main__':
    unittest.main()
