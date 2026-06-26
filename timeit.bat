@echo off
cd /d C:\whp
del /f /q guest.txt 2>nul
start /B launcher_test.exe C:\whp\rootfs /bin/busybox sh -c "echo START=$(date +%s); sleep 3; echo END=$(date +%s); grep -E '^ +0:' /proc/interrupts"
ping -n 9 127.0.0.1 >nul
taskkill /F /IM launcher_test.exe >nul 2>&1
type guest.txt
