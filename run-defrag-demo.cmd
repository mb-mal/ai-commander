@echo off
rem DEFRAG demo: a fake ACP agent that reports usage 53k->41k and streams
rem a compaction. The DEFRAG overlay appears in the open chat.
setlocal
cd /d G:\Dev\ZCodeProject\ai-commander
set AI_COMMANDER_CONFIG_DIR=%TEMP%\aic-demo
set AI_COMMANDER_KANBAN_DB=%TEMP%\aic-demo\kanban.redb
set AI_COMMANDER_COMPACTION_VIEW=defrag
set FAKE_ACP_COMPACTION=1
target\release\ai-commander.exe
endlocal
