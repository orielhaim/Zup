# Application lifecycle

## Install

A fresh installer resolves scope, components, install directory, prerequisites and owned resources, then installs that selection.

## Upgrade

An interactive newer package offers Upgrade for the existing installation. Automation runs `upgrade --scope user` or `upgrade --scope machine`. A lower version is refused.

## Modify

Optional components can change after installation. Resources of removed components leave the owned selection; resources of newly selected ones enter it.

## Repair

Repair restores managed resources that drifted, where that is safe. It is not a reset for user-owned data.

## Update

When `[updates]` is configured, maintenance can check the channel and move to a newer release.

## Uninstall

Uninstall removes resources Zup still owns. Declare resources rather than hiding them in install scripts: only declared resources participate in every operation above.

## Recovery

On Windows and Linux, mandatory completion includes publishing ownership, cleaning transaction backups and owned empty directories, and applying machine-state permissions. These steps are repeatable. Interrupted or failed completion stays pending in the transaction journal; committed application changes are not rolled back. Recovery preserves the original failure and transaction identity until completion succeeds.

Retry a reported transaction with the installer or its maintenance copy:

```sh
./Acme-Setup recover --transaction UUID --scope user --output json
```

Use the original scope. Machine recovery uses the native authorization flow. Console and Headless support the same command and JSON/JSONL output. A GUI installer also accepts this explicit recovery command without opening a window. Keep the reported transaction state directory until recovery finishes. Exit code 7 means recovery is still required.

Uninstall retains its final transaction record so interruption during completion can still be recovered.

Journal validation binds file receipts to the planned destination, digest, size, and executable intent. Ledger publication and directory-ownership transfer use shared policies. Reconciliation validates settled history before cleanup, publishes committed journals in transaction order, and preserves a newer installation when replaying older work.

Windows staging and backup content lives under the installation state's `work/<transaction>` directory. It has no separately supplied work root. Cleanup verifies artifact names, transaction paths, content, and redirect-free paths before deletion. Removal backups must be siblings of the planned owned file.

Completion and failure messages carry closed enums. Unknown wire values are rejected. Diagnostics follow typed outcomes; message wording cannot change success, cancellation, authorization, or recovery classification. A prerequisite requesting a reboot has a separate terminal state and process code 3010. Restart, then run setup again to continue.
