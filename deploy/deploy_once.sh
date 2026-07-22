#!/bin/bash
# One-shot: bounce the paper-trader onto the freshly built binary, then
# remove this job so it never fires again.
launchctl kickstart -k "gui/$(id -u)/com.sid.kalshi-paper-trader"
launchctl bootout "gui/$(id -u)/com.sid.kalshi-deploy-once" 2>/dev/null
rm -f "$HOME/Library/LaunchAgents/com.sid.kalshi-deploy-once.plist"
