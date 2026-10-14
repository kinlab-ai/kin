#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Behavior tests for the optional VFS portion of the Unix installer."""

from __future__ import annotations

import errno
import hashlib
import os
import platform
import pty
import shutil
import stat
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
INSTALLER = ROOT / "scripts" / "install.sh"
VERSION = "9.9.9"


def target_name() -> str:
    system = platform.system()
    machine = platform.machine().lower()
    os_name = {"Darwin": "macos", "Linux": "linux"}[system]
    arch = {
        "x86_64": "x86_64",
        "amd64": "x86_64",
        "arm64": "aarch64",
        "aarch64": "aarch64",
    }[machine]
    return f"{os_name}-{arch}"


# The release kin's registry check, reduced to the rule these tests depend on:
# `registry authority` refuses a Kin home that its group or others can write,
# as the real binary does. Every registry call is recorded beside the home.
REGISTRY_CHECKING_KIN = """#!/bin/sh
if [ "$1" = registry ]; then
  printf '%s\\n' "$*" >> "$KIN_HOME/../registry-calls.log"
  if [ -n "$(find "$KIN_HOME" -prune \\( -perm -020 -o -perm -002 \\) -print)" ]; then
    echo "Error: registry authority initialization refused: registry parent $KIN_HOME must not be group/world writable" >&2
    exit 1
  fi
  exit 0
fi
printf 'kin 9.9.9\\n'
"""


def executable(path: Path, body: str) -> None:
    path.write_text(body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


class OptionalVfsInstallerTests(unittest.TestCase):
    tar_argv_log: Path | None = None

    def run_installer(
        self,
        *,
        vfs_exit: int,
        progress_tty: bool = False,
        pretend_macos: bool = False,
        archive_notifier: bool = True,
        symlinked_notifier_ancestry: bool = False,
        existing_notifier: bool = False,
        seed_current_install: bool = False,
        seed_launcher_stamp: bool = False,
        archive_owner: tuple[int, int] | None = None,
        tar_stub: str | None = None,
        home_files: dict[str, str] | None = None,
        shell: str = "/bin/sh",
        archive_vfs: bool = True,
        seed_stale_projection: bool = False,
        umask: int | None = None,
        group_writable_kin_home: bool = False,
        extra_env: dict[str, str] | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        target = "macos-x86_64" if pretend_macos else target_name()
        archive_root = root / f"kin-{target}"
        archive_root.mkdir()

        executable(archive_root / "kin", REGISTRY_CHECKING_KIN)
        executable(archive_root / "kin-daemon", "#!/bin/sh\nexit 0\n")
        shim_name = (
            "libkin_vfs_shim.dylib"
            if platform.system() == "Darwin"
            else "libkin_vfs_shim.so"
        )
        # v0.7.18 and later publish no projection runtime at all, so the
        # installer has to be exercised against both archive shapes.
        if archive_vfs:
            executable(
                archive_root / "kin-vfs",
                f"#!/bin/sh\nexit {vfs_exit}\n",
            )
            (archive_root / shim_name).write_bytes(b"test shim")
        if target.startswith("macos-") and archive_notifier:
            bundle_root = archive_root / "KinNotifier.app"
            notifier = bundle_root / (
                "Payload" if symlinked_notifier_ancestry else "Contents"
            )
            (notifier / "MacOS").mkdir(parents=True)
            (notifier / "Resources").mkdir()
            executable(
                notifier / "MacOS" / "KinNotifier",
                "#!/bin/sh\nprintf 'new-notifier\\n'\n",
            )
            (notifier / "Info.plist").write_text("<plist>new</plist>", encoding="utf-8")
            if symlinked_notifier_ancestry:
                (bundle_root / "Contents").symlink_to("Payload", target_is_directory=True)

        release_dir = root / "download" / f"v{VERSION}"
        release_dir.mkdir(parents=True)
        archive = release_dir / f"kin-{target}.tar.gz"
        def stamp_archive_owner(info: tarfile.TarInfo) -> tarfile.TarInfo:
            if archive_owner is not None:
                info.uid, info.gid = archive_owner
                # Empty names force tar to honour the numeric ids, which is what
                # a release archive built on a CI runner effectively carries.
                info.uname = ""
                info.gname = ""
            return info

        with tarfile.open(archive, "w:gz") as bundle:
            bundle.dereference = False
            bundle.add(
                archive_root, arcname=archive_root.name, filter=stamp_archive_owner
            )
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        archive.with_suffix(archive.suffix + ".sha256").write_text(
            f"{digest}  {archive.name}\n", encoding="utf-8"
        )

        home = root / "home"
        home.mkdir()
        for name, body in (home_files or {}).items():
            (home / name).write_text(body, encoding="utf-8")
        kin_home = home / ".kin"
        if seed_current_install:
            (kin_home / "bin").mkdir(parents=True)
            executable(
                kin_home / "bin" / "kin",
                "#!/bin/sh\n# old-kin\nprintf 'kin 9.9.9\\n'\n",
            )
            executable(kin_home / "bin" / "kin-daemon", "#!/bin/sh\nprintf 'old-daemon\\n'\n")
        if seed_stale_projection:
            (kin_home / "bin").mkdir(parents=True, exist_ok=True)
            (kin_home / "lib").mkdir(parents=True, exist_ok=True)
            executable(kin_home / "bin" / "kin-vfs", "#!/bin/sh\nexit 0\n")
            (kin_home / "lib" / shim_name).write_bytes(b"stale shim")
        if seed_launcher_stamp:
            (kin_home / "bin").mkdir(parents=True, exist_ok=True)
            (kin_home / "bin" / ".kinlab-kin-version").write_text(
                "8.8.8\n", encoding="utf-8"
            )
        if group_writable_kin_home:
            (kin_home / "bin").mkdir(parents=True, exist_ok=True)
            kin_home.chmod(0o775)
        if existing_notifier:
            old_contents = kin_home / "lib" / "KinNotifier.app" / "Contents"
            (old_contents / "MacOS").mkdir(parents=True)
            executable(
                old_contents / "MacOS" / "KinNotifier",
                "#!/bin/sh\nprintf 'old-notifier\\n'\n",
            )
            (old_contents / "Info.plist").write_text("<plist>old</plist>", encoding="utf-8")
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(home),
                "KIN_BASE_URL": root.as_uri(),
                "KIN_HOME": str(kin_home),
                "KIN_NO_SETUP": "1",
                "KIN_VERSION": VERSION,
                "SHELL": shell,
            }
        )
        env.update(extra_env or {})
        fake_bin = root / "fake-bin"
        if pretend_macos or tar_stub is not None:
            fake_bin.mkdir(exist_ok=True)
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
        if pretend_macos:
            executable(
                fake_bin / "uname",
                "#!/bin/sh\n"
                "case \"${1:-}\" in\n"
                "  -s) printf 'Darwin\\n' ;;\n"
                "  -m) printf 'x86_64\\n' ;;\n"
                "  *) exit 2 ;;\n"
                "esac\n",
            )
        if tar_stub is not None:
            real_tar = shutil.which("tar", path=os.defpath)
            assert real_tar is not None, "no system tar to delegate to"
            self.tar_argv_log = root / "tar-argv.log"
            # "reject" stands in for busybox tar, which spells no-same-owner `-o`
            # and refuses the long option outright, so the installer's fallback
            # is exercised rather than assumed.
            refusal = (
                'for arg in "$@"; do\n'
                '  if [ "$arg" = "--no-same-owner" ]; then\n'
                '    echo "tar: unrecognized option: no-same-owner" >&2\n'
                "    exit 1\n"
                "  fi\n"
                "done\n"
                if tar_stub == "reject"
                else ""
            )
            executable(
                fake_bin / "tar",
                "#!/bin/sh\n"
                f'printf \'%s\\n\' "$*" >> "{self.tar_argv_log}"\n'
                f"{refusal}"
                f'exec "{real_tar}" "$@"\n',
            )
        args = ["sh", str(INSTALLER)]
        if progress_tty:
            master, slave = pty.openpty()
            try:
                process = subprocess.Popen(
                    args,
                    env=env,
                    stdout=subprocess.PIPE,
                    stderr=slave,
                    text=True,
                )
            finally:
                os.close(slave)

            stderr_chunks: list[bytes] = []
            try:
                while True:
                    try:
                        chunk = os.read(master, 4096)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if not chunk:
                        break
                    stderr_chunks.append(chunk)
            finally:
                os.close(master)
            stdout, _ = process.communicate()
            result = subprocess.CompletedProcess(
                args,
                process.returncode,
                stdout,
                b"".join(stderr_chunks).decode("utf-8", errors="replace"),
            )
        else:
            result = subprocess.run(
                args,
                env=env,
                check=False,
                capture_output=True,
                text=True,
                preexec_fn=None if umask is None else (lambda: os.umask(umask)),
            )
        return result, kin_home

    def login_shell_path(self, home: Path) -> str:
        """What PATH a real `bash -lc` carries in this home."""

        probe = subprocess.run(
            ["/bin/bash", "-lc", 'printf \'%s\\n\' "$PATH"'],
            env={"HOME": str(home), "PATH": "/usr/bin:/bin"},
            cwd=str(home),
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(probe.returncode, 0, probe.stderr)
        return probe.stdout.strip()

    def test_installer_puts_kin_on_path_for_a_bash_login_shell(self) -> None:
        """FIR-2596. bash reads .bashrc only for an interactive non-login shell.

        A login shell reads .bash_profile, .bash_login or .profile, the first
        one only, and never .bashrc, so the installer writing .bashrc alone left
        `bash -lc 'command -v kin'` empty on a fresh install. The assertion is
        on the install's own bin directory rather than on which kin wins,
        because a login shell runs /etc/profile first and macOS's path_helper
        puts the system directories back in front.
        """

        result, kin_home = self.run_installer(
            vfs_exit=0, shell="/bin/bash", home_files={".bashrc": "# mine\n"}
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        home = kin_home.parent
        bin_dir = str(kin_home / "bin")

        profile = home / ".bash_profile"
        self.assertTrue(
            profile.is_file(),
            "a home with no login file gets one, or a login shell reads nothing",
        )
        profile_text = profile.read_text(encoding="utf-8")
        self.assertIn(bin_dir, profile_text)
        self.assertIn(
            "case $- in",
            profile_text,
            "the created file pairs with ~/.bashrc only when interactive; "
            "unguarded it would source the projection hook into every bash -lc",
        )
        self.assertIn(
            bin_dir,
            (home / ".bashrc").read_text(encoding="utf-8"),
            "an interactive non-login bash reads only .bashrc, so the line stays "
            "there too",
        )

        if not Path("/bin/bash").is_file():
            return
        self.assertIn(
            bin_dir,
            self.login_shell_path(home).split(":"),
            "a real bash login shell still does not carry the install's bin "
            "directory",
        )

        # Falsification: the pre-fix layout, .bashrc and nothing else.
        profile.unlink()
        self.assertNotIn(
            bin_dir,
            self.login_shell_path(home).split(":"),
            "with the login file gone a login shell still carries the bin "
            "directory, so this check cannot fail and is not evidence",
        )

    def test_installer_appends_to_the_login_file_bash_would_read(self) -> None:
        """bash stops at the first login file that exists, so the installer has
        to append to that one rather than create a file bash will skip."""

        result, kin_home = self.run_installer(
            vfs_exit=0,
            shell="/bin/bash",
            home_files={".bashrc": "# mine\n", ".profile": "# mine\n"},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        home = kin_home.parent
        bin_dir = str(kin_home / "bin")

        profile_text = (home / ".profile").read_text(encoding="utf-8")
        self.assertTrue(
            profile_text.startswith("# mine\n"),
            f"an existing login file keeps what its owner put there: {profile_text}",
        )
        self.assertIn(bin_dir, profile_text)
        self.assertFalse(
            (home / ".bash_profile").exists(),
            "creating .bash_profile beside an existing .profile takes over "
            "which file bash reads, which is not the installer's call",
        )

    def test_installer_writes_no_bash_file_for_a_home_that_runs_zsh(self) -> None:
        """The control. A bash arm that fires unconditionally would pass the two
        checks above while conjuring dotfiles in every zsh user's home."""

        result, kin_home = self.run_installer(
            vfs_exit=0, shell="/bin/zsh", home_files={".zshrc": "# mine\n"}
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        home = kin_home.parent
        for name in (".bashrc", ".bash_profile", ".bash_login", ".profile"):
            self.assertFalse(
                (home / name).exists(),
                f"the installer created {name} in a home that runs zsh",
            )
        self.assertIn(
            str(kin_home / "bin"),
            (home / ".zshenv").read_text(encoding="utf-8"),
            "the zsh arm still has to do its own job",
        )

    def test_a_usergroups_umask_still_installs_with_private_directories(self) -> None:
        """Ubuntu's default umask, 0002, made ~/.kin group writable.

        The registry check then refused the directory and the site's first
        command failed on every distribution that sets USERGROUPS_ENAB. The
        installer now takes group and world write out of its own umask.
        """

        result, kin_home = self.run_installer(vfs_exit=0, umask=0o002)

        self.assertEqual(result.returncode, 0, result.stderr)
        for directory in (kin_home, kin_home / "bin", kin_home / "lib"):
            self.assertEqual(
                stat.S_IMODE(directory.stat().st_mode),
                0o755,
                f"{directory} under umask 0002",
            )
        self.assertEqual(
            (kin_home.parent / "registry-calls.log").read_text(encoding="utf-8"),
            "registry authority --initialize\n",
        )

    def test_a_stricter_umask_is_kept(self) -> None:
        result, kin_home = self.run_installer(vfs_exit=0, umask=0o077)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(stat.S_IMODE(kin_home.stat().st_mode), 0o700)

    def test_a_group_writable_kin_home_is_named_with_its_exact_repair(self) -> None:
        """A home an earlier install left at 0775 is never repaired silently.

        The refusal names the directory and the command that repairs it, and
        nothing installed is replaced.
        """

        result, kin_home = self.run_installer(
            vfs_exit=0,
            umask=0o002,
            group_writable_kin_home=True,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"chmod go-w '{kin_home}'", result.stderr)
        self.assertIn("drwxrwxr-x", result.stderr)
        self.assertIn("KIN_REGISTRY_REPAIR=1", result.stderr)
        self.assertNotIn("For permission-only drift", result.stderr)
        self.assertEqual(stat.S_IMODE(kin_home.stat().st_mode), 0o775)
        self.assertFalse((kin_home / "bin" / "kin").exists())

    def test_registry_repair_consent_covers_a_group_writable_kin_home(self) -> None:
        result, kin_home = self.run_installer(
            vfs_exit=0,
            umask=0o002,
            group_writable_kin_home=True,
            extra_env={"KIN_REGISTRY_REPAIR": "1"},
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(stat.S_IMODE(kin_home.stat().st_mode), 0o755)
        self.assertIn(f"Removed group and world write from {kin_home}", result.stdout)
        self.assertEqual(
            (kin_home.parent / "registry-calls.log").read_text(encoding="utf-8"),
            "registry authority --fix --initialize\n",
        )

    def test_reports_projection_only_when_vfs_is_executable(self) -> None:
        result, kin_home = self.run_installer(vfs_exit=0)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("% Total", result.stderr)
        self.assertIn("Filesystem projection installed", result.stdout)
        self.assertTrue((kin_home / "bin" / "kin-vfs").exists())

    def test_removes_and_reports_unusable_projection(self) -> None:
        result, kin_home = self.run_installer(vfs_exit=127)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("Filesystem projection installed", result.stdout)
        self.assertIn("Filesystem projection is unavailable", result.stdout)
        self.assertFalse((kin_home / "bin" / "kin-vfs").exists())
        self.assertFalse(any((kin_home / "lib").glob("libkin_vfs_shim.*")))

    def test_an_archive_without_projection_installs_and_says_so(self) -> None:
        """The shape every release has published since v0.7.18.

        This is also the documented recovery for an install whose updater still
        requires `kin-vfs` and therefore refuses the release outright, so the
        installer has to finish cleanly on an archive that carries neither the
        projection client nor its shim.
        """

        result, kin_home = self.run_installer(vfs_exit=0, archive_vfs=False)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("not bundled in this archive", result.stdout)
        self.assertTrue((kin_home / "bin" / "kin").exists())
        self.assertTrue((kin_home / "bin" / "kin-daemon").exists())
        self.assertFalse((kin_home / "bin" / "kin-vfs").exists())

    def test_existing_projection_is_not_removed_outside_native_transaction(self) -> None:
        result, kin_home = self.run_installer(
            vfs_exit=0, archive_vfs=False, seed_current_install=True,
            seed_stale_projection=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Pinned or mirrored reinstalls", result.stderr)
        self.assertTrue((kin_home / "bin" / "kin-vfs").exists())
        self.assertTrue(any((kin_home / "lib").glob("libkin_vfs_shim.*")))
        self.assertIn("old-daemon", (kin_home / "bin" / "kin-daemon").read_text())

    def test_extraction_refuses_the_archives_recorded_ownership(self) -> None:
        # tar running as root restores the uid/gid the archive records, and
        # release archives are built by CI under an unrelated uid. Left alone,
        # a root install lands a foreign-owned projection shim in the user's own
        # ~/.kin, Kin refuses to verify the shim it just installed, and doctor
        # reports the install ledger STALE on a clean install.
        result, _ = self.run_installer(vfs_exit=0, tar_stub="record")

        self.assertEqual(result.returncode, 0, result.stderr)
        assert self.tar_argv_log is not None
        invocations = self.tar_argv_log.read_text(encoding="utf-8")
        self.assertIn(
            "--no-same-owner",
            invocations,
            f"the installer must not let tar restore archived ownership: {invocations!r}",
        )

    def test_extraction_falls_back_when_tar_rejects_no_same_owner(self) -> None:
        result, kin_home = self.run_installer(vfs_exit=0, tar_stub="reject")

        self.assertEqual(result.returncode, 0, result.stderr)
        assert self.tar_argv_log is not None
        invocations = self.tar_argv_log.read_text(encoding="utf-8").splitlines()
        self.assertTrue(
            any("--no-same-owner" in line for line in invocations),
            f"the long option is still tried first: {invocations!r}",
        )
        self.assertTrue(
            any("--no-same-owner" not in line for line in invocations),
            f"a tar that refuses the option must still extract: {invocations!r}",
        )
        self.assertTrue((kin_home / "bin" / "kin").exists())
        self.assertTrue((kin_home / "bin" / "kin-daemon").exists())

    @unittest.skipUnless(
        hasattr(os, "geteuid") and os.geteuid() == 0,
        "only root is handed the archive's recorded ownership by tar",
    )
    def test_root_install_lands_every_file_owned_by_the_installing_user(self) -> None:
        result, kin_home = self.run_installer(vfs_exit=0, archive_owner=(1001, 1001))

        self.assertEqual(result.returncode, 0, result.stderr)
        installed = [
            kin_home / "bin" / "kin",
            kin_home / "bin" / "kin-daemon",
            kin_home / "bin" / "kin-vfs",
            *(kin_home / "lib").glob("libkin_vfs_shim.*"),
        ]
        self.assertGreaterEqual(len(installed), 4)
        for path in installed:
            self.assertEqual(
                path.stat().st_uid,
                os.geteuid(),
                f"{path} kept the archive's uid instead of the installing user's",
            )

    def test_interactive_archive_download_exposes_live_byte_percent_meter(self) -> None:
        result, _ = self.run_installer(vfs_exit=0, progress_tty=True)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("% Total", result.stderr)
        self.assertIn("% Received", result.stderr)

    def test_same_version_pin_cannot_repair_bundles_outside_native_transaction(self) -> None:
        result, kin_home = self.run_installer(
            vfs_exit=0, pretend_macos=True, seed_current_install=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Pinned or mirrored reinstalls", result.stderr)
        self.assertIn("old-kin", (kin_home / "bin" / "kin").read_text())
        self.assertFalse((kin_home / "lib" / "KinNotifier.app").exists())

    def test_malformed_macos_archive_refuses_before_replacing_existing_install(self) -> None:
        result, kin_home = self.run_installer(
            vfs_exit=0,
            pretend_macos=True,
            archive_notifier=False,
            existing_notifier=True,
            seed_current_install=True,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Pinned or mirrored reinstalls", result.stderr)
        self.assertIn(
            "kin 9.9.9",
            (kin_home / "bin" / "kin").read_text(encoding="utf-8"),
        )
        self.assertIn(
            "old-notifier",
            (
                kin_home
                / "lib"
                / "KinNotifier.app"
                / "Contents"
                / "MacOS"
                / "KinNotifier"
            ).read_text(encoding="utf-8"),
        )

    def test_symlinked_bundle_ancestor_refuses_before_replacing_install_or_stamp(self) -> None:
        result, kin_home = self.run_installer(
            vfs_exit=0,
            pretend_macos=True,
            symlinked_notifier_ancestry=True,
            existing_notifier=True,
            seed_current_install=True,
            seed_launcher_stamp=True,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Pinned or mirrored reinstalls", result.stderr)
        self.assertEqual(
            (kin_home / "bin" / "kin").read_text(encoding="utf-8"),
            "#!/bin/sh\n# old-kin\nprintf 'kin 9.9.9\\n'\n",
        )
        self.assertEqual(
            (kin_home / "bin" / "kin-daemon").read_text(encoding="utf-8"),
            "#!/bin/sh\nprintf 'old-daemon\\n'\n",
        )
        self.assertEqual(
            (
                kin_home
                / "lib"
                / "KinNotifier.app"
                / "Contents"
                / "MacOS"
                / "KinNotifier"
            ).read_text(encoding="utf-8"),
            "#!/bin/sh\nprintf 'old-notifier\\n'\n",
        )
        self.assertEqual(
            (
                kin_home / "lib" / "KinNotifier.app" / "Contents" / "Info.plist"
            ).read_text(encoding="utf-8"),
            "<plist>old</plist>",
        )
        self.assertEqual(
            (kin_home / "bin" / ".kinlab-kin-version").read_text(encoding="utf-8"),
            "8.8.8\n",
        )

    def test_fresh_bootstrap_rejects_symlinked_notification_bundle_before_native_bytes(self) -> None:
        result, kin_home = self.run_installer(
            vfs_exit=0, pretend_macos=True, symlinked_notifier_ancestry=True,
            existing_notifier=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("contains a symlink or special entry", result.stderr)
        self.assertFalse((kin_home / "bin" / "kin").exists())
        self.assertIn("old-notifier", (kin_home / "lib" / "KinNotifier.app" / "Contents" / "MacOS" / "KinNotifier").read_text())


if __name__ == "__main__":
    unittest.main()
