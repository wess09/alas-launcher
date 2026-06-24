; ==========================================================================
;  AzurPilot Installer Script
;  Inno Setup 7 · Admin privileges · 简体中文
; ==========================================================================

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#ifndef OutputBaseFilename
  #define OutputBaseFilename "AzurPilot_Setup"
#endif

#ifndef PackageRoot
  #define PackageRoot "."
#endif

#ifndef SetupRoot
  #define SetupRoot "setup"
#endif

[Setup]
AppName=AzurPilot
AppVersion={#AppVersion}
AppPublisher=AzurPilot Team
AppId={{1A779131-3DD5-067C-0ABC-E656396F6879}

DefaultDirName={autopf}\AzurPilot
AppendDefaultDirName=yes
DefaultGroupName=AzurPilot
OutputDir=.
OutputBaseFilename={#OutputBaseFilename}
Compression=lzma
SolidCompression=yes

ArchitecturesAllowed=x86 x64
ArchitecturesInstallIn64BitMode=x64
DisableProgramGroupPage=yes
WizardStyle=modern
ShowLanguageDialog=no
MinVersion=10.0
ExtraDiskSpaceRequired=1510725938

PrivilegesRequired=admin
UninstallDisplayIcon={app}\alas-launcher.exe

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

; --------------------------------------------------------------------------
;  目录权限：Inno 自带 Permissions 做首轮设置，[Code] 中 icacls 做兜底修复
; --------------------------------------------------------------------------
[Dirs]
Name: "{app}";          Permissions: users-modify
Name: "{app}\config";   Permissions: users-modify
Name: "{app}\deploy";   Permissions: users-modify
Name: "{app}\.venv";    Permissions: users-modify
Name: "{app}\bootstrap";  Permissions: users-modify


; --------------------------------------------------------------------------
;  应用文件 + 运行时安装器
; --------------------------------------------------------------------------
[Files]
; 应用本体
Source: "{#PackageRoot}\alas-launcher.exe";  DestDir: "{app}";          Flags: ignoreversion; Permissions: users-modify
Source: "{#PackageRoot}\config\*";           DestDir: "{app}\config";   Flags: ignoreversion recursesubdirs createallsubdirs; Permissions: users-modify
Source: "{#PackageRoot}\deploy\*";           DestDir: "{app}\deploy";   Flags: ignoreversion recursesubdirs createallsubdirs; Permissions: users-modify
Source: "{#PackageRoot}\bootstrap\*";        DestDir: "{app}\bootstrap";  Flags: ignoreversion recursesubdirs createallsubdirs; Permissions: users-modify

; 运行时安装器（释放到临时目录，安装后自动清理）
Source: "{#SetupRoot}\MicrosoftEdgeWebview2Setup.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall skipifsourcedoesntexist
Source: "{#SetupRoot}\vcredist_x64.exe";               DestDir: "{tmp}"; Flags: deleteafterinstall skipifsourcedoesntexist; Check: IsWin64
Source: "{#SetupRoot}\vcredist_x86.exe";               DestDir: "{tmp}"; Flags: deleteafterinstall skipifsourcedoesntexist

[Icons]
Name: "{autoprograms}\AzurPilot"; Filename: "{app}\alas-launcher.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\AzurPilot";  Filename: "{app}\alas-launcher.exe"; WorkingDir: "{app}"; Tasks: desktopicon

; --------------------------------------------------------------------------
;  安装后执行：VC++ / WebView2 / 启动器
; --------------------------------------------------------------------------
[Run]
; VC++ 2015-2022：幂等操作，已有相同或更高版本时安装器秒退
Filename: "{tmp}\vcredist_x64.exe"; \
  Parameters: "/install /quiet /norestart"; \
  StatusMsg: "正在从塞壬主服务器里偷取最新的运行环境 (x64)..."; \
  Flags: waituntilterminated skipifdoesntexist; \
  Check: IsWin64

Filename: "{tmp}\vcredist_x86.exe"; \
  Parameters: "/install /quiet /norestart"; \
  StatusMsg: "正在从塞壬主服务器里偷取最新的运行环境 (x86)..."; \
  Flags: waituntilterminated skipifdoesntexist

; WebView2 Bootstrapper：联网检测+安装，可能耗时较长
Filename: "{tmp}\MicrosoftEdgeWebview2Setup.exe"; \
  Parameters: "/silent /install"; \
  StatusMsg: "正在向明石支付800红宝石以解锁WebView2的下载带宽（可能需要几分钟）..."; \
  Flags: waituntilterminated skipifdoesntexist

Filename: "{app}\alas-launcher.exe"; \
  Description: "{cm:LaunchProgram,AzurPilot}"; \
  WorkingDir: "{app}"; \
  Flags: nowait postinstall skipifsilent

[Code]

// ==========================================================================
//  用户协议 / 隐私声明（内嵌网页）
//  通过 ATL ActiveX Host 创建 AtlAxWin 子窗口，嵌入本地 wrapper HTML。
//  Wrapper 内用 iframe 加载线上协议页，强制 scrolling="yes" 保证滚动条可用。
// ==========================================================================
const
  WS_CHILD = $40000000;
  WS_VISIBLE = $10000000;
  WS_BORDER = $00800000;
  WS_CLIPSIBLINGS = $04000000;
  WS_TABSTOP = $00010000;
  AGREEMENT_URL = 'https://alas.nanoda.work/WVSb8fdprkNr8pEWnVS56XxxrpRtS7tVpj8b5Bs8A242Z8K8hv7rPdbsZmsX/privacydisclaimer.html';

function AtlAxWinInit(): Boolean;
  external 'AtlAxWinInit@atl.dll cdecl';

function CreateWindowEx(dwExStyle: LongWord; lpClassName, lpWindowName: String;
  dwStyle: LongWord; x, y, nWidth, nHeight: Integer;
  hWndParent, hMenu, hInstance: HWND; lpParam: LongWord): HWND;
#ifdef UNICODE
  external 'CreateWindowExW@user32.dll stdcall';
#else
  external 'CreateWindowExA@user32.dll stdcall';
#endif

function DestroyWindow(hWnd: HWND): Boolean;
  external 'DestroyWindow@user32.dll stdcall';

function SetFocus(hWnd: HWND): HWND;
  external 'SetFocus@user32.dll stdcall';

function GetTickCount: Cardinal;
  external 'GetTickCount@kernel32.dll stdcall';

// ==========================================================================
//  全局变量
// ==========================================================================
var
  AgreementPage: TWizardPage;
  AgreementCheck: TNewCheckBox;
  AgreementBrowser: HWND;
  AgreementWrapperPath: String;
  AgreementOpenButton: TNewButton;

  ETALabel: TNewStaticText;
  InstallStartTick: Cardinal;
  PermStartTick: Cardinal;

  KeepDeployYaml: Boolean;
  DeployYamlBackupPath: String;

// ==========================================================================
//  用户协议页面
// ==========================================================================
procedure UpdateAgreementNextButton;
begin
  if Assigned(AgreementPage) and (WizardForm.CurPageID = AgreementPage.ID) then
    WizardForm.NextButton.Enabled := AgreementCheck.Checked;
end;

procedure AgreementCheckClick(Sender: TObject);
begin
  UpdateAgreementNextButton;
end;

function HtmlEscapeForAttr(const S: String): String;
begin
  Result := S;
  StringChangeEx(Result, '&', '&amp;', True);
  StringChangeEx(Result, '"', '&quot;', True);
  StringChangeEx(Result, '<', '&lt;', True);
  StringChangeEx(Result, '>', '&gt;', True);
end;

function BuildAgreementWrapperHtml: String;
begin
  Result :=
    '<!doctype html>' + #13#10 +
    '<html>' + #13#10 +
    '<head>' + #13#10 +
    '  <meta http-equiv="X-UA-Compatible" content="IE=edge" />' + #13#10 +
    '  <meta charset="utf-8" />' + #13#10 +
    '  <style>' + #13#10 +
    '    html, body { width:100%; height:100%; margin:0; padding:0; overflow:hidden; background:#fff; }' + #13#10 +
    '    #wrap { position:absolute; left:0; top:0; right:0; bottom:0; overflow-y:scroll; overflow-x:hidden; -ms-overflow-style:scrollbar; }' + #13#10 +
    '    iframe { display:block; width:100%; height:6000px; border:0; overflow:hidden; }' + #13#10 +
    '  </style>' + #13#10 +
    '</head>' + #13#10 +
    '<body>' + #13#10 +
    '  <div id="wrap">' + #13#10 +
    '    <iframe src="' + HtmlEscapeForAttr(AGREEMENT_URL) + '" scrolling="yes"></iframe>' + #13#10 +
    '  </div>' + #13#10 +
    '</body>' + #13#10 +
    '</html>';
end;

function PrepareAgreementWrapper: Boolean;
begin
  AgreementWrapperPath := ExpandConstant('{tmp}\azurpilot_agreement_wrapper.html');
  Result := SaveStringToFile(AgreementWrapperPath, BuildAgreementWrapperHtml, False);
end;

function LocalPathToFileUrl(const Path: String): String;
begin
  Result := Path;
  StringChangeEx(Result, '\', '/', True);
  StringChangeEx(Result, ' ', '%20', True);
  Result := 'file:///' + Result;
end;

procedure AgreementOpenButtonClick(Sender: TObject);
var
  ResultCode: Integer;
begin
  ShellExec('open', AGREEMENT_URL, '', '', SW_SHOWNORMAL, ewNoWait, ResultCode);
end;

procedure CreateAgreementPage;
var
  BrowserTop: Integer;
  BrowserHeight: Integer;
  BrowserUrl: String;
begin
  AgreementPage := CreateCustomPage(
    wpSelectDir,
    '用户协议与隐私声明',
    '请阅读页面内容，勾选同意后继续安装 AzurPilot。'
  );

  BrowserTop := 0;
  BrowserHeight := AgreementPage.SurfaceHeight - ScaleY(64);

  if PrepareAgreementWrapper then
    BrowserUrl := LocalPathToFileUrl(AgreementWrapperPath)
  else
    BrowserUrl := AGREEMENT_URL;

  if AtlAxWinInit() then
  begin
    AgreementBrowser := CreateWindowEx(
      0,
      'AtlAxWin',
      BrowserUrl,
      WS_CHILD or WS_VISIBLE or WS_BORDER or WS_CLIPSIBLINGS or WS_TABSTOP,
      ScaleX(0),
      BrowserTop,
      AgreementPage.SurfaceWidth,
      BrowserHeight,
      AgreementPage.Surface.Handle,
      0, 0, 0
    );
  end;

  AgreementCheck := TNewCheckBox.Create(AgreementPage);
  AgreementCheck.Parent := AgreementPage.Surface;
  AgreementCheck.Left := ScaleX(0);
  AgreementCheck.Top := AgreementPage.SurfaceHeight - ScaleY(28);
  AgreementCheck.Width := AgreementPage.SurfaceWidth;
  AgreementCheck.Height := ScaleY(22);
  AgreementCheck.Caption := '我已阅读并同意用户协议与隐私声明';
  AgreementCheck.Checked := False;
  AgreementCheck.OnClick := @AgreementCheckClick;
end;

// ==========================================================================
//  向导初始化
// ==========================================================================
procedure InitializeWizard;
begin
  CreateAgreementPage;

  ETALabel := TNewStaticText.Create(WizardForm);
  ETALabel.Parent := WizardForm.InstallingPage;
  ETALabel.Left := WizardForm.ProgressGauge.Left;
  ETALabel.Top := WizardForm.ProgressGauge.Top + WizardForm.ProgressGauge.Height + ScaleY(4);
  ETALabel.Width := WizardForm.ProgressGauge.Width;
  ETALabel.Caption := '';
end;

// ==========================================================================
//  向导页面切换
// ==========================================================================
procedure CurPageChanged(CurPageID: Integer);
var
  FocusedWindow: HWND;
begin
  if Assigned(AgreementPage) and (CurPageID = AgreementPage.ID) then
  begin
    WizardForm.NextButton.Enabled := AgreementCheck.Checked;
    if AgreementBrowser <> 0 then
      FocusedWindow := SetFocus(AgreementBrowser);
  end
  else
    WizardForm.NextButton.Enabled := True;
end;

procedure DeinitializeSetup;
var
  Destroyed: Boolean;
begin
  if AgreementBrowser <> 0 then
    Destroyed := DestroyWindow(AgreementBrowser);
end;

// ==========================================================================
//  安装路径校验
//  拦截 Windows 目录、裸 Program Files 根目录、磁盘根目录等危险路径，
//  避免递归 ACL 修改影响系统文件。
// ==========================================================================
function CanonicalDir(const Path: String): String;
begin
  Result := RemoveBackslashUnlessRoot(Path);
end;

function StartsWithPath(const Full, Prefix: String): Boolean;
var
  F, P: String;
begin
  F := CanonicalDir(Full);
  P := CanonicalDir(Prefix);
  Result :=
    (CompareText(F, P) = 0) or
    (
      (Length(F) > Length(P)) and
      (CompareText(Copy(F, 1, Length(P)), P) = 0) and
      (F[Length(P) + 1] = '\')
    );
end;

function IsDriveRootPath(const Path: String): Boolean;
var
  P: String;
begin
  P := AddBackslash(CanonicalDir(Path));
  Result := (Length(P) = 3) and (P[2] = ':') and (P[3] = '\');
end;

function IsBareProgramFilesPath(const Path: String): Boolean;
var
  P: String;
begin
  P := CanonicalDir(Path);
  Result :=
    (CompareText(P, CanonicalDir(ExpandConstant('{autopf}'))) = 0) or
    (CompareText(P, CanonicalDir(ExpandConstant('{commonpf32}'))) = 0);
  if Result then Exit;
  if IsWin64 then
    Result := CompareText(P, CanonicalDir(ExpandConstant('{commonpf64}'))) = 0;
end;

function AddAzurPilotIfBareContainer(const Dir: String): String;
var
  D: String;
begin
  D := CanonicalDir(Dir);
  Result := D;
  if IsBareProgramFilesPath(D) or IsDriveRootPath(D) then
    Result := AddBackslash(D) + 'AzurPilot';
end;

function IsUnderProgramFilesPath(const Path: String): Boolean;
begin
  Result := StartsWithPath(Path, ExpandConstant('{commonpf32}'));
  if Result then Exit;
  if IsWin64 then
    Result := StartsWithPath(Path, ExpandConstant('{commonpf64}'));
end;

function IsUnderProgramFiles: Boolean;
begin
  Result := IsUnderProgramFilesPath(ExpandConstant('{app}'));
end;

function IsUnderWindowsPath(const Path: String): Boolean;
begin
  Result := StartsWithPath(Path, ExpandConstant('{win}'));
end;

function IsUnderWindowsDir: Boolean;
begin
  Result := IsUnderWindowsPath(ExpandConstant('{app}'));
end;

function IsUnsafeInstallPath(const Path: String): Boolean;
begin
  Result := IsUnderWindowsPath(Path) or IsBareProgramFilesPath(Path) or IsDriveRootPath(Path);
end;

function IsUnsafeInstallDir: Boolean;
begin
  Result := IsUnsafeInstallPath(ExpandConstant('{app}'));
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  Dir, FixedDir: String;
begin
  Result := True;

  if Assigned(AgreementPage) and (CurPageID = AgreementPage.ID) then
  begin
    if not AgreementCheck.Checked then
    begin
      MsgBox('指挥官，请先阅读并同意用户协议与隐私声明后再出港。', mbError, MB_OK);
      Result := False;
      Exit;
    end;
  end;

  if CurPageID = wpSelectDir then
  begin
    Dir := CanonicalDir(WizardDirValue);

    if IsUnderWindowsPath(Dir) then
    begin
      MsgBox(
        '指挥官，这里是禁区！不能在受保护目录安装：' + #13#10 +
        Dir + #13#10#13#10 +
        '请改用默认目录，或选择例如 D:\Apps\AzurPilot',
        mbError, MB_OK
      );
      Result := False;
      Exit;
    end;

    FixedDir := AddAzurPilotIfBareContainer(Dir);
    if CompareText(FixedDir, Dir) <> 0 then
    begin
      WizardForm.DirEdit.Text := FixedDir;
      Result := False;
      Exit;
    end;
  end;
end;

// ==========================================================================
//  进程清理
// ==========================================================================
procedure KillProcessByImageName(const ImageName: String);
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'),
       '/F /T /IM ' + ImageName,
       '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure StopRunningProcesses;
begin
  KillProcessByImageName('alas-launcher.exe');
  KillProcessByImageName('pythonw.exe');
  KillProcessByImageName('python.exe');
  KillProcessByImageName('git.exe');
  KillProcessByImageName('git-remote-http.exe');
  KillProcessByImageName('git-remote-https.exe');
  Sleep(3000);
end;

// ==========================================================================
//  安装前准备
//  路径安全校验 → deploy.yaml 备份 → 清理残留进程
// ==========================================================================
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  DeployYamlPath: String;
begin
  Result := '';
  KeepDeployYaml := False;

  if IsUnsafeInstallDir then
  begin
    Result := '指挥官，这个位置太危险了：' + ExpandConstant('{app}') + #13#10 +
              '请不要安装到 Windows 目录、Program Files 根目录或磁盘根目录。';
    Exit;
  end;

  DeployYamlPath := ExpandConstant('{app}\config\deploy.yaml');
  if FileExists(DeployYamlPath) then
  begin
    KeepDeployYaml := MsgBox(
      '女仆长发现你的配置文件 config\deploy.yaml 还在原位！' + #13#10#13#10 +
      '是否保留现有配置？' + #13#10 +
      '点击「是」保留，点击「否」恢复为出厂默认配置。',
      mbConfirmation,
      MB_YESNO or MB_DEFBUTTON1
    ) = IDYES;

    if KeepDeployYaml then
    begin
      DeployYamlBackupPath := ExpandConstant('{tmp}\deploy.yaml.bak');
      FileCopy(DeployYamlPath, DeployYamlBackupPath, False);
    end;
  end;

  StopRunningProcesses;
end;

// ==========================================================================
//  安装进度 & 预计剩余时间
//  文件释放阶段：通过 CurInstallProgressChanged 回调，基于已用时间和百分比推算 ETA
//  权限修复阶段：通过 RunPermStep 逐步推进进度条，同步计算 ETA
//  ETA 通过独立的 ETALabel 控件显示，不覆盖 Inno 原生状态文本
// ==========================================================================
const
  PERM_TOTAL = 10;

function FormatETA(Seconds: Integer): String;
begin
  if Seconds < 0 then
    Seconds := 0;
  if Seconds < 60 then
    Result := IntToStr(Seconds) + ' 秒'
  else if Seconds < 3600 then
    Result := IntToStr(Seconds div 60) + ' 分 ' + IntToStr(Seconds mod 60) + ' 秒'
  else
    Result := IntToStr(Seconds div 3600) + ' 小时 ' + IntToStr((Seconds mod 3600) div 60) + ' 分';
end;

procedure RunPermStep(Step: Integer; const Msg, Filename, Params: String);
var
  ResultCode: Integer;
  ElapsedSec, ETASec, TotalEstSec: Integer;
begin
  WizardForm.ProgressGauge.Position := Step;
  WizardForm.StatusLabel.Caption :=
    '正在配置目录权限 [' + IntToStr(Step + 1) + '/' + IntToStr(PERM_TOTAL) + ']';
  WizardForm.FilenameLabel.Caption := Msg;

  if Step > 0 then
  begin
    ElapsedSec := (GetTickCount - PermStartTick) div 1000;
    if ElapsedSec > 0 then
    begin
      TotalEstSec := (ElapsedSec * PERM_TOTAL) div Step;
      ETASec := TotalEstSec - ElapsedSec;
      if ETASec < 0 then ETASec := 0;
      ETALabel.Caption := '预计剩余 ' + FormatETA(ETASec);
    end;
  end
  else
    ETALabel.Caption := '';

  Exec(Filename, Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

// ==========================================================================
//  目录权限修复（ssPostInstall 阶段）
//  attrib 清只读属性 → icacls 重置/继承/授权
//  仅对 {app} 目录生效，不影响系统目录
// ==========================================================================
procedure RunPermissionSteps;
var
  AppDir, SysDir: String;
begin
  if IsUnsafeInstallDir then
    Exit;

  AppDir := ExpandConstant('{app}');
  SysDir := ExpandConstant('{sys}');

  WizardForm.ProgressGauge.Min := 0;
  WizardForm.ProgressGauge.Max := PERM_TOTAL;
  WizardForm.ProgressGauge.Position := 0;
  PermStartTick := GetTickCount;

  RunPermStep(0, '皇家方舟正在试图获取驱逐舰宿舍的最高访问权限...',
    SysDir + '\attrib.exe', '-R "' + AppDir + '\*" /S /D');

  RunPermStep(1, '大凤正在将指挥官的浏览器历史记录悄悄打包...',
    SysDir + '\icacls.exe', '"' + AppDir + '" /setowner *S-1-5-32-544 /T /C');

  RunPermStep(2, '赤城正在把其他后台进程当成『害虫』扔进焚化炉...',
    SysDir + '\icacls.exe', '"' + AppDir + '" /reset /T /C');

  RunPermStep(3, '正在给小贝法喂食，以防她看到我们在改C盘然后报警...',
    SysDir + '\icacls.exe', '"' + AppDir + '" /inheritance:e /T /C');

  RunPermStep(4, '13-4的防空炮火太猛了，安装程序正在紧急规避...',
    SysDir + '\icacls.exe',
    '"' + AppDir + '" /remove:d *S-1-1-0 *S-1-5-11 *S-1-5-32-545 *S-1-15-2-1 *S-1-15-2-2 /T /C');

  RunPermStep(5, '皇家方舟正在申请前往驱逐舰宿舍的通行证...',
    SysDir + '\icacls.exe', '"' + AppDir + '" /grant:r *S-1-1-0:(OI)(CI)F /T /C');

  RunPermStep(6, '大凤正在偷偷配制您电脑的管理员钥匙...',
    SysDir + '\icacls.exe', '"' + AppDir + '" /grant:r *S-1-5-11:(OI)(CI)F /T /C');

  RunPermStep(7, '天狼星不小心弄乱了临时文件，正在慌张地清扫...',
    SysDir + '\icacls.exe', '"' + AppDir + '" /grant:r *S-1-5-32-545:(OI)(CI)F /T /C');

  RunPermStep(8, '拉菲正在睡觉...',
    SysDir + '\icacls.exe', '"' + AppDir + '" /grant:r *S-1-15-2-1:(OI)(CI)F /T /C');

  RunPermStep(9, '光辉正在将爱与和平洒满港区...',
    SysDir + '\icacls.exe', '"' + AppDir + '" /grant:r *S-1-15-2-2:(OI)(CI)F /T /C');

  WizardForm.ProgressGauge.Position := PERM_TOTAL;
end;

// ==========================================================================
//  安装步骤回调
//  ssPostInstall：权限修复 → deploy.yaml 还原
// ==========================================================================
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    RunPermissionSteps;

    if KeepDeployYaml and (DeployYamlBackupPath <> '') then
      FileCopy(DeployYamlBackupPath, ExpandConstant('{app}\config\deploy.yaml'), False);
  end;
end;

// ==========================================================================
//  文件释放阶段 ETA
//  节流 500ms + 指数移动平均（7:3 权重）防止数值跳动
// ==========================================================================
var
  LastETAUpdateTick: Cardinal;
  SmoothedETA: Integer;

procedure CurInstallProgressChanged(CurProgress, MaxProgress: Integer);
var
  Now: Cardinal;
  ElapsedSec, Pct, RawETA: Integer;
begin
  if InstallStartTick = 0 then
  begin
    InstallStartTick := GetTickCount;
    LastETAUpdateTick := 0;
    SmoothedETA := -1;
  end;

  Now := GetTickCount;

  if (Now - LastETAUpdateTick) < 500 then
    Exit;

  ElapsedSec := (Now - InstallStartTick) div 1000;

  if (CurProgress > 0) and (MaxProgress > 0) and (ElapsedSec >= 1) then
  begin
    Pct := (CurProgress * 100) div MaxProgress;
    if Pct > 0 then
    begin
      RawETA := ((ElapsedSec * 100) div Pct) - ElapsedSec;
      if RawETA < 0 then RawETA := 0;

      if SmoothedETA < 0 then
        SmoothedETA := RawETA
      else
        SmoothedETA := (SmoothedETA * 7 + RawETA * 3) div 10;

      LastETAUpdateTick := Now;
      ETALabel.Caption := '预计剩余 ' + FormatETA(SmoothedETA);
    end;
  end;
end;

// ==========================================================================
//  卸载流程
//  停止残留进程 → 清理桌面快捷方式 → 询问是否保留数据 → 删除目录
// ==========================================================================
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  KeepData: Boolean;
  DesktopPath: String;
begin
  case CurUninstallStep of

    usUninstall:
      StopRunningProcesses;

    usPostUninstall:
      begin
        DesktopPath := ExpandConstant('{autodesktop}\AzurPilot.lnk');
        if FileExists(DesktopPath) then
          DeleteFile(DesktopPath);
        DesktopPath := ExpandConstant('{commondesktop}\AzurPilot.lnk');
        if FileExists(DesktopPath) then
          DeleteFile(DesktopPath);
        DesktopPath := ExpandConstant('{userdesktop}\AzurPilot.lnk');
        if FileExists(DesktopPath) then
          DeleteFile(DesktopPath);

        KeepData :=
          MsgBox(
            '是否保留 AzurPilot 的配置和数据文件？不知火可以先帮你收进仓库。' + #13#10 +
            ExpandConstant('{app}') + #13#10#13#10 +
            '点击「是」保留数据，点击「否」让蛮啾清空这个目录。',
            mbConfirmation,
            MB_YESNO or MB_DEFBUTTON1
          ) = IDYES;

        if not KeepData then
        begin
          if DirExists(ExpandConstant('{app}')) then
            DelTree(ExpandConstant('{app}'), True, True, True);
          if DirExists(ExpandConstant('{app}')) then
            RemoveDir(ExpandConstant('{app}'));
        end;
      end;

  end;
end;
