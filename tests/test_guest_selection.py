"""Exercise guest selection without downloads, compilation, or virtualization."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


RUN = Path(__file__).with_name('run.sh').resolve()


class GuestSelection(unittest.TestCase):
    def exercise(self, mode, runner_exit=0):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            commands = root / 'commands'
            commands.mkdir()
            tests = root / 'tests'
            (tests / 'target/debug').mkdir(parents=True)
            # Reused runners can have both environment variables and cached assets.
            (root / 'freebsd-sysroot').mkdir()
            (root / 'freebsd-sysroot/.sysroot_ready').touch()
            (root / 'init').mkdir()
            (root / 'init/init-freebsd').touch()
            log = root / 'calls'
            def executable(path, body):
                path.write_text('#!/bin/sh\n' + body)
                path.chmod(0o700)
            executable(commands / 'cargo', 'echo "cargo $*" >> "$CALL_LOG"\n')
            executable(commands / 'curl', 'echo curl >> "$CALL_LOG"\nexit 19\n')
            executable(commands / 'uname', 'case "$1" in -s) echo Linux;; *) echo x86_64;; esac\n')
            executable(tests / 'target/debug/runner',
                       'echo "runner kernel=${KRUN_TEST_FREEBSD_KERNEL_PATH-unset} iso=${KRUN_TEST_FREEBSD_ISO_PATH-unset}" >> "$CALL_LOG"\n'
                       f'exit {runner_exit}\n')
            env = dict(os.environ, PATH=str(commands) + ':' + os.environ['PATH'],
                       CALL_LOG=str(log), KRUN_TEST_FREEBSD=mode,
                       KRUN_TEST_FREEBSD_KERNEL_PATH=str(root / 'missing-kernel'),
                       KRUN_TEST_FREEBSD_ISO_PATH=str(root / 'missing-iso'),
                       KRUN_TEST_GVPROXY_PATH='/unused-explicit-proxy')
            # No sysroot in required-mode control: fail clearly, not by compiling.
            if mode == '1':
                (root / 'freebsd-sysroot/.sysroot_ready').unlink()
            result = subprocess.run(['sh', str(RUN)], cwd=tests, env=env,
                                    capture_output=True, text=True, timeout=10)
            return result, log.read_text() if log.exists() else ''

    def test_linux_has_no_freebsd_download_build_or_stale_assets(self):
        result, calls = self.exercise('0')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('curl', calls)
        self.assertNotIn('unknown-freebsd', calls)
        self.assertIn('unknown-linux-musl', calls)
        self.assertIn('runner kernel=unset iso=unset', calls)

    def test_linux_runner_failure_is_not_hidden(self):
        result, calls = self.exercise('0', runner_exit=7)
        self.assertEqual(result.returncode, 7)
        self.assertIn('runner', calls)

    def test_required_freebsd_cannot_pass_without_assets(self):
        result, calls = self.exercise('1')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('dedicated FreeBSD tests require', result.stdout)
        self.assertNotIn('runner kernel=', calls)

    def test_invalid_mode_fails_before_build_or_download(self):
        result, calls = self.exercise('typo')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, '')


if __name__ == '__main__':
    unittest.main()
