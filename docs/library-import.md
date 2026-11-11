# Library import recovery

A library import updates the selected `pous/lib/<library>/*.st` files and
the library's entry in `project.toml`. The stored TOML values for other
libraries, governance and unknown fields survive a successful import.
A rollback restores the original bytes of every file it replaced.

## Commit and recovery

1. Under the project lock, IA2 validates the destination paths and stages
   new contents, copies of existing files, and a text journal. The journal
   records the transaction format, file names, content hashes and access
   metadata.
   An ownership marker binds the journal to its project directory. A
   journal copied to another project is refused even if the files match
   the recorded hashes.
2. IA2 publishes the fully prepared directory as `.ia2-library-import/`
   and syncs the project root before replacing a live file. It syncs each
   replacement and its directory. The manifest is installed last.
3. IA2 verifies the installed files, renames `prepared.toml` to
   `committed.toml` and syncs the journal directory. Successful import
   responses and library mutation events follow this commit point.
4. IA2 renames the finished journal out of the active path before deleting
   scratch contents. If cleanup fails after commit, the new files remain
   authoritative and cleanup can resume on reopen.

A reopened project with a valid prepared journal rolls back. A valid
committed journal keeps the installed version and resumes cleanup.
Version 1 journals lack the access information required for this check.
IA2 retains those journals and reports that recovery needs intervention.
Recovery is safe to retry
after interruption. If the commit marker was renamed but syncing it failed,
the import reports an unconfirmed outcome. Reopen and read back the version
and selected POU files to resolve it.

If a file matches neither recorded version, or a path or journal is invalid,
IA2 preserves the conflicting file and recovery data. Project source snapshots
and deployment preflight fail until the conflict is resolved. Save the
project and its recovery directory before manual intervention. Restoring
a verified original or installed file lets IA2 retry automatic recovery.

## Filesystem boundary

IA2 coordinates cooperating processes with a project-level filesystem lock.
Each operation explicitly unlocks before closing its descriptor, so a
concurrent child-process launch cannot retain a finished operation's lock.
POU snapshots and deployment copies either obtain consistent inputs or
report busy/recovery. Compilation uses those copied inputs after releasing
the lock. Recovery paths stay in the project and reject symlinked targets
or library directories. Deployment omits the root transaction paths.

An existing library directory and its selected files must have the requested
names on disk. Two registered names must resolve to separate directories.
IA2 checks these rules against the project filesystem for import, removal
and recovery. Names that resolve to separate directories can coexist.

On macOS and Linux, IA2 checks the mode, owner and group of each replacement
and refuses extended file ACLs before changing an installed file. If an
ACL appears during a pending import, recovery keeps that file and its
journal for inspection. On Windows, the read-only flag and a fingerprint
of the owner, group, DACL and mandatory label must match the staged
replacement. A permission query error stops the operation.
[Darwin ACL API](https://raw.githubusercontent.com/apple-oss-distributions/Libc/main/posix1e/acl_file.c)
· [Linux extended attributes](https://man7.org/linux/man-pages/man7/xattr.7.html)
· [Windows file security descriptors](https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-getsecurityinfo)

Use a local filesystem with atomic same-volume rename and reliable file
locking. On Unix the project directory serves as the lock and directory
sync errors are surfaced. Where the standard file API lacks directory
sync, the protocol covers process interruption with the platform's file
sync and rename semantics. Such platforms need a writable project at first
open to create the persistent `.ia2-library-lock` file. Power-loss behavior
depends on the filesystem and storage. External editors and older IA2
versions do not take this lock. Avoid changing transaction files or
replacing directories concurrently, and finish recovery on the same
filesystem before moving the project.
