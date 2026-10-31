"""Generate the Android lib-name patch for git's run-command.c.

Applied by scripts/build-android-git.sh. Run from WSL:
  python3 scripts/patches/make-git-android-patch.py SRC_GIT_DIR OUT.patch
"""

import os
import subprocess
import sys

HELPER = '''#ifdef __ANDROID__
/*
 * Android only lets an app execute files that live in the APK's native library
 * directory, and that directory cannot hold a name Gradle would not package
 * (lib*.so). Git still looks its own programs up by bare name, so map a name
 * that PATH did not resolve onto the packaged lib<name>.so beside the binary.
 */
static char *android_lib_program(const char *name)
{
\tstruct strbuf candidate = STRBUF_INIT;
\tchar *resolved = NULL;

\tstrbuf_addf(&candidate, "%s/lib%s.so", git_exec_path(), name);
\tif (access(candidate.buf, X_OK) == 0)
\t\tresolved = xstrdup(candidate.buf);
\tstrbuf_release(&candidate);
\treturn resolved;
}
#endif

static int prepare_cmd(struct strvec *out, const struct child_process *cmd)
'''

ANCHOR = 'static int prepare_cmd(struct strvec *out, const struct child_process *cmd)\n'

OLD_ELSE = '''\t\t} else {
\t\t\tstrvec_clear(out);
\t\t\terrno = ENOENT;
\t\t\treturn -1;
\t\t}
'''

NEW_ELSE = '''\t\t} else {
\t\t\tchar *packaged = NULL;
#ifdef __ANDROID__
\t\t\tpackaged = android_lib_program(out->v[1]);
#endif
\t\t\tif (packaged) {
\t\t\t\tfree((char *)out->v[1]);
\t\t\t\tout->v[1] = packaged;
\t\t\t} else {
\t\t\t\tstrvec_clear(out);
\t\t\t\terrno = ENOENT;
\t\t\t\treturn -1;
\t\t\t}
\t\t}
'''


def main() -> None:
    src, out = sys.argv[1], sys.argv[2]
    path = os.path.join(src, "run-command.c")
    with open(path) as fh:
        text = fh.read()
    if "android_lib_program" in text:
        sys.exit("run-command.c is already patched")
    if text.count(ANCHOR) != 1 or text.count(OLD_ELSE) != 1:
        sys.exit("run-command.c does not match the expected shape")
    text = text.replace(ANCHOR, HELPER).replace(OLD_ELSE, NEW_ELSE)
    edited = path + ".android"
    with open(edited, "w") as fh:
        fh.write(text)
    diff = subprocess.run(
        ["diff", "-u", path, edited], capture_output=True, text=True
    ).stdout
    os.remove(edited)
    lines = []
    for line in diff.splitlines():
        if line.startswith("--- "):
            lines.append("--- a/run-command.c")
        elif line.startswith("+++ "):
            lines.append("+++ b/run-command.c")
        else:
            lines.append(line)
    with open(out, "w") as fh:
        fh.write("\n".join(lines) + "\n")
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
