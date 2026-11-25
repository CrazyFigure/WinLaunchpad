Unicode true
RequestExecutionLevel user
!include "MUI2.nsh"

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\WinLaunchpad"

Name "WinLaunchpad"
OutFile "${SETUP_EXE}"
InstallDir "$LOCALAPPDATA\Programs\WinLaunchpad"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
SetCompressor /SOLID lzma
VIProductVersion "${PE_VERSION}"
VIAddVersionKey /LANG=2052 "ProductName" "WinLaunchpad"
VIAddVersionKey /LANG=2052 "FileDescription" "WinLaunchpad 安装程序"
VIAddVersionKey /LANG=2052 "CompanyName" "CrazyFigure"
VIAddVersionKey /LANG=2052 "FileVersion" "${APP_VERSION}"
VIAddVersionKey /LANG=2052 "ProductVersion" "${APP_VERSION}"
VIAddVersionKey /LANG=2052 "LegalCopyright" "Copyright (C) CrazyFigure"

!define MUI_ICON "..\assets\app.ico"
!define MUI_WELCOMEPAGE_TITLE "安装 WinLaunchpad"
!define MUI_WELCOMEPAGE_TEXT "将 WinLaunchpad 安装到当前用户账户，并创建开始菜单入口。$\r$\n$\r$\n点击「下一步」选择安装位置。"
!define MUI_FINISHPAGE_RUN "$INSTDIR\WinLaunchpad.exe"
!define MUI_FINISHPAGE_RUN_TEXT "打开 WinLaunchpad"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

; 安装或卸载前让用户从托盘退出，避免覆盖正在运行的程序，也避免旧便携版接管安装后的启动命令。
!macro RequireClosed
retry:
    FindWindow $0 "WinLaunchpadMain"
    StrCmp $0 0 ready
    IfSilent blocked
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "请先从系统托盘退出 WinLaunchpad，再点击「重试」。" IDRETRY retry
blocked:
    SetErrorLevel 2
    Abort
ready:
!macroend

Section "WinLaunchpad"
    !insertmacro RequireClosed
    SetShellVarContext current
    SetOutPath "$INSTDIR"
    File /oname=WinLaunchpad.exe "${APP_EXE}"
    WriteUninstaller "$INSTDIR\Uninstall.exe"
    CreateShortCut "$SMPROGRAMS\WinLaunchpad.lnk" "$INSTDIR\WinLaunchpad.exe"

    ; 仅注册当前用户的卸载入口，允许通过 Windows「已安装的应用」卸载。
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "WinLaunchpad"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${APP_VERSION}"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "CrazyFigure"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\WinLaunchpad.exe"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '$\"$INSTDIR\Uninstall.exe$\"'
    WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '$\"$INSTDIR\Uninstall.exe$\" /S'
    WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
    WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1

    ; 已有便携版启动项跟随安装位置，配置文件继续由程序自行读写。
    ExecWait '"$INSTDIR\WinLaunchpad.exe" --installer-refresh-startup' $0
    StrCmp $0 0 +2
    DetailPrint "已有开机启动项未能更新，可在 WinLaunchpad 设置中重新启用。"
SectionEnd

Section "Uninstall"
    !insertmacro RequireClosed
    SetShellVarContext current
    IfFileExists "$INSTDIR\WinLaunchpad.exe" 0 cleanup
    ExecWait '"$INSTDIR\WinLaunchpad.exe" --installer-remove-startup' $0
    StrCmp $0 0 cleanup
    DetailPrint "开机启动项未能清理。"

cleanup:
    ; 只删除安装器创建的程序和入口。设置、缓存及目录中的其他文件可以保留。
    Delete "$SMPROGRAMS\WinLaunchpad.lnk"
    Delete "$INSTDIR\WinLaunchpad.exe"
    Delete "$INSTDIR\Uninstall.exe"
    DeleteRegKey HKCU "${UNINSTALL_KEY}"
    RMDir "$INSTDIR"
SectionEnd
