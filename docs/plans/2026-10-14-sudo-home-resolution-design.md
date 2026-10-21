# Resolve home directories only from a real sudo context

A non-root process with HOME=/home/user can inherit SUDO_USER=root (or another identity) from a container shell. Previously real_home_dir preferred that identity regardless of effective UID, potentially selecting inaccessible /root data.

On Unix, consult SUDO_USER only when the effective UID is zero and the name is nonempty and not root. The passwd result must also have a nonzero UID. Otherwise use the existing dirs::home_dir fallback. Non-Unix behavior and explicit ATOMCODE_HOME precedence are unchanged. Do not change permissions, migrate data or silently switch an explicitly configured product directory.

Pure resolver tests cover ordinary users with stale sudo identities, a root process resolving a non-root caller, missing/root/empty identities and passwd lookup failure. Existing CLI error_report and session permission diagnostics already deduplicate Permission denied and suggest an explicit data directory; no second diagnostic path is added. This fixes a proven resolver boundary, not proof that the reported user's environment had a stale SUDO_USER.
