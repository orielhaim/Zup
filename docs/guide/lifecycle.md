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

On Linux, recovery retries failed cleanup of old maintenance generations after commit. A cleanup failure keeps completion pending; committed application changes are not rolled back.
