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

On Linux, mandatory completion includes publishing ownership, cleaning transaction backups and owned empty directories, and applying machine-state permissions. These steps are repeatable. Interrupted or failed completion stays pending in the transaction journal; committed application changes are not rolled back. Recovery preserves the original failure and transaction identity until completion succeeds.

Retry a reported transaction with the installer or its maintenance copy:

```sh
./Acme-Setup recover --transaction UUID --scope user --output json
```

Use the original scope. Machine recovery uses the native authorization flow. Console and Headless support the same command and JSON/JSONL output. A GUI installer also accepts this explicit recovery command without opening a window. Keep the reported transaction state directory until recovery finishes. Exit code 7 means recovery is still required.

Uninstall retains its final transaction record so interruption during completion can still be recovered.
