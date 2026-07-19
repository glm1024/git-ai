import * as vscode from "vscode";
import {
  normalizeReportingSettings,
  validateReportingSettings,
  type ReportingSettings,
} from "./reporting-profile-utils";
import {
  ReportingProfileService,
  type InitialReportingState,
  type OrganizationLoadResult,
} from "./reporting-profile-service";

export const REPORTING_PROFILE_VIEW_ID = "git-ai.reportingProfile";

export class ReportingProfileViewProvider implements vscode.WebviewViewProvider {
  private view: vscode.WebviewView | undefined;
  private organization: OrganizationLoadResult | undefined;
  private requestId = 0;

  public constructor(private readonly service: ReportingProfileService) {}

  public resolveWebviewView(webviewView: vscode.WebviewView): void {
    this.view = webviewView;
    webviewView.title = "";
    webviewView.webview.options = { enableScripts: true };
    webviewView.webview.html = getHtml(webviewView.webview);
    webviewView.webview.onDidReceiveMessage((message: unknown) => this.handleMessage(message));
  }

  public async reveal(): Promise<void> {
    this.view?.show?.(true);
    await this.postInitialState();
  }

  private async handleMessage(message: unknown): Promise<void> {
    if (!isMessage(message)) {
      return;
    }
    switch (message.type) {
      case "ready":
        await this.postInitialState();
        return;
      case "serverChanged":
        await this.postOrganization(message.metricsApiBaseUrl);
        return;
      case "save":
        await this.save(message.settings);
        return;
      default:
        return;
    }
  }

  private async postInitialState(): Promise<void> {
    const requestId = ++this.requestId;
    const initial = await this.service.loadInitialState();
    if (requestId !== this.requestId || !this.view) {
      return;
    }
    this.organization = initial.organization;
    await this.postMessage({ type: "initial", initial });
  }

  private async postOrganization(metricsApiBaseUrl: string): Promise<void> {
    const requestId = ++this.requestId;
    const organization = await this.service.loadOrganizationOptions(metricsApiBaseUrl);
    if (requestId !== this.requestId || !this.view) {
      return;
    }
    this.organization = organization;
    await this.postMessage({ type: "organization", organization });
  }

  private async save(settings: ReportingSettings): Promise<void> {
    const error = validateReportingSettings(settings, this.organization?.options);
    if (error) {
      await this.postMessage({ type: "saveResult", ok: false, error });
      return;
    }
    try {
      const saved = await this.service.save(normalizeReportingSettings(settings));
      await this.postMessage({ type: "saveResult", ok: true, settings: saved });
    } catch (saveError) {
      await this.postMessage({
        type: "saveResult",
        ok: false,
        error: saveError instanceof Error ? saveError.message : "保存失败，请稍后重试",
      });
    }
  }

  private async postMessage(message: unknown): Promise<void> {
    await this.view?.webview.postMessage(message);
  }
}

function isMessage(message: unknown): message is {
  type: "ready" | "serverChanged" | "save";
  metricsApiBaseUrl: string;
  settings: ReportingSettings;
} {
  if (!message || typeof message !== "object" || !("type" in message)) {
    return false;
  }
  const type = (message as { type?: unknown }).type;
  if (type === "ready") {
    return true;
  }
  if (type === "serverChanged") {
    return typeof (message as { metricsApiBaseUrl?: unknown }).metricsApiBaseUrl === "string";
  }
  if (type !== "save") {
    return false;
  }
  const settings = (message as { settings?: unknown }).settings;
  if (!settings || typeof settings !== "object") {
    return false;
  }
  const profile = (settings as { profile?: unknown }).profile;
  return typeof (settings as { metricsApiBaseUrl?: unknown }).metricsApiBaseUrl === "string"
    && Boolean(profile)
    && typeof profile === "object";
}

function getHtml(webview: vscode.Webview): string {
  const nonce = getNonce();
  const csp = `default-src 'none'; style-src ${webview.cspSource} 'unsafe-inline'; script-src 'nonce-${nonce}';`;
  return `<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <meta http-equiv="Content-Security-Policy" content="${csp}" />
  <title>Git AI 数据上报设置</title>
  <style>
    :root { color: var(--vscode-foreground); font-family: var(--vscode-font-family); font-size: var(--vscode-font-size); }
    * { box-sizing: border-box; }
    body { margin: 0; background: var(--vscode-sideBar-background); color: var(--vscode-foreground); }
    main { min-height: 100vh; padding: 18px 16px 24px; }
    .page-header { display: flex; align-items: center; justify-content: space-between; gap: 16px; margin: 0 0 22px; }
    h1 { margin: 0; font-size: 18px; font-weight: 600; }
    .header-actions { display: inline-flex; align-items: center; gap: 12px; }
    .save-state { display: inline-flex; align-items: center; gap: 6px; color: var(--vscode-descriptionForeground); font-size: 12px; white-space: nowrap; }
    .save-state::before { content: ""; width: 7px; height: 7px; border-radius: 999px; background: var(--vscode-descriptionForeground); }
    .save-state.saved { color: #72d69a; }.save-state.saved::before { background: #72d69a; }
    .save-state.dirty { color: #e7b864; }.save-state.dirty::before { background: #e7b864; }
    .save-state.error { color: var(--vscode-errorForeground); }.save-state.error::before { background: var(--vscode-errorForeground); }
    .header-rule { height: 1px; margin: 0 0 18px; background: rgba(59, 130, 246, 0.72); }
    .field-cluster { margin: 0 0 18px; padding: 0 0 18px; border-bottom: 1px solid rgba(59, 130, 246, 0.72); }
    .field { display: grid; gap: 6px; margin: 0 0 14px; }
    .field:last-child { margin-bottom: 0; }
    label { font-weight: 500; }
    input { width: 100%; min-height: 28px; border: 1px solid var(--vscode-input-border, transparent); border-radius: 2px; padding: 4px 7px; background: var(--vscode-input-background); color: var(--vscode-input-foreground); font: inherit; transition: border-color 140ms ease, box-shadow 140ms ease, transform 140ms ease; }
    input:focus, button:focus { outline: 1px solid var(--vscode-focusBorder); outline-offset: 1px; }
    input:disabled, button:disabled { opacity: 0.6; }
    .help { color: var(--vscode-descriptionForeground); font-size: 12px; line-height: 1.45; }
    .select-control { position: relative; }
    .select-control::after { content: ""; position: absolute; right: 12px; top: calc(50% - 5px); width: 8px; height: 8px; border-right: 1.5px solid var(--vscode-dropdown-foreground, var(--vscode-input-foreground)); border-bottom: 1.5px solid var(--vscode-dropdown-foreground, var(--vscode-input-foreground)); opacity: 0.5; pointer-events: none; transform: rotate(45deg); transition: transform 140ms ease; }
    .select-trigger { display: block; width: 100%; min-height: 28px; border: 1px solid var(--vscode-dropdown-border, var(--vscode-input-border, transparent)); border-radius: 2px; padding: 4px 34px 4px 12px; background: var(--vscode-dropdown-background, var(--vscode-input-background)); color: var(--vscode-dropdown-foreground, var(--vscode-input-foreground)); font: inherit; text-align: left; cursor: pointer; transition: color 140ms ease, box-shadow 140ms ease; }
    .select-trigger:hover:not(:disabled) { background: transparent; }
    .select-control.is-open .select-trigger { border-color: var(--vscode-focusBorder); }
    .select-control.is-open::after { transform: translateY(4px) rotate(225deg); }
    .select-control.is-selected .select-trigger { animation: select-settle 180ms ease-out; }
    @keyframes select-settle { 50% { border-color: var(--vscode-focusBorder); } }
    .select-menu { position: absolute; z-index: 20; top: calc(100% + 4px); left: 0; width: 100%; max-height: 288px; overflow-y: auto; padding: 4px; border: 1px solid var(--vscode-focusBorder); border-radius: 2px; background: var(--vscode-dropdown-background, var(--vscode-editorWidget-background)); color: var(--vscode-dropdown-foreground, var(--vscode-foreground)); box-shadow: 0 2px 5px rgba(0, 0, 0, 0.3); transform-origin: top; animation: menu-open 150ms ease-out; }
    .select-menu[hidden] { display: none; }
    @keyframes menu-open { from { opacity: 0; transform: translateY(-2px) scale(0.95); } to { opacity: 1; transform: translateY(0) scale(1); } }
    .select-option { position: relative; display: block; width: 100%; min-height: 30px; border: 0; border-radius: 2px; padding: 6px 32px 6px 8px; color: var(--vscode-dropdown-foreground, var(--vscode-foreground)); background: transparent; font: inherit; font-size: 12px; text-align: left; cursor: pointer; }
    .select-option:hover, .select-option:focus { color: var(--vscode-list-activeSelectionForeground); background: var(--vscode-list-activeSelectionBackground); outline: none; }
    .select-option.selected { color: var(--vscode-list-activeSelectionForeground); background: var(--vscode-list-activeSelectionBackground); }
    .select-check { position: absolute; right: 8px; top: 50%; display: inline-flex; width: 14px; height: 14px; align-items: center; justify-content: center; transform: translateY(-50%); font-size: 13px; }
    .select-option.invalid { color: var(--vscode-descriptionForeground); }
    button { display: inline-flex; align-items: center; justify-content: center; gap: 7px; min-height: 28px; border: 0; border-radius: 2px; padding: 0 12px; color: var(--vscode-button-foreground); background: var(--vscode-button-background); font: inherit; cursor: pointer; transition: background 140ms ease, transform 140ms ease; }
    button:hover:not(:disabled) { background: var(--vscode-button-hoverBackground); }
    button:disabled { opacity: 0.6; cursor: not-allowed; }
    button:active:not(:disabled) { transform: translateY(1px); }
    #save { min-width: 104px; padding: 0 24px; color: #1f1f1f; font-weight: 400; }
    @media (prefers-reduced-motion: reduce) { *, *::before, *::after { animation-duration: 1ms !important; transition-duration: 1ms !important; } }
    #team-field[hidden] { display: none; }
  </style>
</head>
<body>
  <main>
    <div class="page-header"><h1>数据上报</h1><div class="header-actions"><div id="save-state" class="save-state saved" role="status" aria-live="polite">已保存</div><button id="save" type="button"><span id="save-label">保存</span></button></div></div>
    <div class="header-rule"></div>
    <div class="field-cluster">
      <div class="field"><label for="department">部门</label><div class="select-control"><button id="department" class="select-trigger" type="button" role="combobox" aria-expanded="false" aria-controls="department-menu"><span id="department-value">请选择部门</span></button><div id="department-menu" class="select-menu" role="listbox" hidden></div></div></div>
      <div class="field"><label for="office">处</label><div class="select-control"><button id="office" class="select-trigger" type="button" role="combobox" aria-expanded="false" aria-controls="office-menu"><span id="office-value">请先选择部门</span></button><div id="office-menu" class="select-menu" role="listbox" hidden></div></div></div>
      <div class="field" id="team-field"><label for="team">组</label><div class="select-control"><button id="team" class="select-trigger" type="button" role="combobox" aria-expanded="false" aria-controls="team-menu"><span id="team-value">请选择组</span></button><div id="team-menu" class="select-menu" role="listbox" hidden></div></div></div>
    </div>
    <div class="field-cluster">
      <div class="field"><label for="user-name">姓名</label><input id="user-name" autocomplete="name" /></div>
      <div class="field"><label for="user-email">公司邮箱</label><input id="user-email" type="email" inputmode="email" autocomplete="email" /></div>
    </div>
    <div class="field"><label for="server-url">上报服务器地址</label><input id="server-url" type="url" inputmode="url" spellcheck="false" /><div class="help">用于接收 AI 生成代码统计数据。</div></div>
  </main>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const state = { settings: { metricsApiBaseUrl: '', profile: { departmentName: '', officeName: '', teamName: '', userName: '', userEmail: '' } }, organization: undefined, cliError: '', saveError: '', saving: false, dirty: false, openMenu: '' };
    let serverTimer;
    const element = (id) => document.getElementById(id);
    const setSaveState = (text, kind) => { const node = element('save-state'); node.textContent = text; node.className = 'save-state ' + kind; };
    const updateSelect = (id, placeholder, items, selected) => {
      const trigger = element(id); const value = element(id + '-value'); const menu = element(id + '-menu');
      value.textContent = selected || placeholder; menu.replaceChildren();
      const choices = selected && !items.includes(selected) ? [selected, ...items] : items;
      for (const item of choices) {
        const option = document.createElement('button'); option.type = 'button'; option.className = 'select-option' + (item === selected ? ' selected' : '') + (item === selected && !items.includes(item) ? ' invalid' : ''); option.setAttribute('role', 'option'); option.setAttribute('aria-selected', String(item === selected)); const label = document.createElement('span'); label.textContent = item + (item === selected && !items.includes(item) ? '（已失效）' : ''); option.append(label); if (item === selected) { const check = document.createElement('span'); check.className = 'select-check'; check.textContent = '✓'; option.append(check); } option.addEventListener('click', () => chooseOption(id, item)); menu.append(option);
      }
      const isOpen = state.openMenu === id && !trigger.disabled; trigger.setAttribute('aria-expanded', String(isOpen)); trigger.parentElement.classList.toggle('is-open', isOpen); menu.hidden = !isOpen;
    };
    const offices = () => state.organization?.options?.departments.find((department) => department.name === state.settings.profile.departmentName)?.offices || [];
    const teams = () => offices().find((office) => office.name === state.settings.profile.officeName)?.teams || [];
    const renderSaveState = () => {
      if (state.cliError) setSaveState(state.cliError, 'error');
      else if (state.saveError) setSaveState(state.saveError, 'error');
      else if (state.organization?.error) setSaveState(state.organization.error, 'error');
      else if (state.dirty) setSaveState('有未保存更改', 'dirty');
      else if (state.organization?.source === 'cache') setSaveState('正在使用缓存', 'dirty');
      else setSaveState('已保存', 'saved');
    };
    const render = () => {
      const options = state.organization?.options;
      updateSelect('department', '请选择部门', options?.departments.map((department) => department.name) || [], state.settings.profile.departmentName);
      updateSelect('office', state.settings.profile.departmentName ? '请选择处' : '请先选择部门', offices().map((office) => office.name), state.settings.profile.officeName);
      const availableTeams = teams(); const teamField = element('team-field'); teamField.hidden = availableTeams.length === 0;
      if (availableTeams.length === 0 && state.settings.profile.teamName) { state.settings.profile.teamName = ''; state.saveError = ''; state.dirty = true; }
      updateSelect('team', '请选择组', availableTeams, state.settings.profile.teamName);
      element('office').disabled = !state.settings.profile.departmentName || !options;
      element('team').disabled = !state.settings.profile.officeName || !options;
      element('user-name').value = state.settings.profile.userName;
      element('user-email').value = state.settings.profile.userEmail;
      element('server-url').value = state.settings.metricsApiBaseUrl;
      element('save').disabled = Boolean(state.cliError);
      element('save-label').textContent = '保存';
      renderSaveState();
    };
    const markDirty = () => { state.dirty = true; state.saveError = ''; };
    const chooseOption = (id, value) => {
      if (id === 'department') { state.settings.profile.departmentName = value; state.settings.profile.officeName = ''; state.settings.profile.teamName = ''; }
      if (id === 'office') { state.settings.profile.officeName = value; state.settings.profile.teamName = ''; }
      if (id === 'team') state.settings.profile.teamName = value;
      state.openMenu = ''; markDirty(); render(); const control = element(id).parentElement; control.classList.add('is-selected'); setTimeout(() => control.classList.remove('is-selected'), 220);
    };
    const toggleMenu = (id) => { if (element(id).disabled) return; state.openMenu = state.openMenu === id ? '' : id; render(); };
    for (const id of ['department', 'office', 'team']) {
      element(id).addEventListener('click', () => toggleMenu(id));
      element(id).addEventListener('keydown', (event) => { if (event.key === 'Escape') { state.openMenu = ''; render(); } else if (event.key === 'ArrowDown' || event.key === 'Enter' || event.key === ' ') { event.preventDefault(); state.openMenu = id; render(); } });
    }
    document.addEventListener('pointerdown', (event) => { if (state.openMenu && !event.target.closest('.select-control')) { state.openMenu = ''; render(); } });
    element('user-name').addEventListener('input', (event) => { state.settings.profile.userName = event.target.value; markDirty(); renderSaveState(); });
    element('user-email').addEventListener('input', (event) => { state.settings.profile.userEmail = event.target.value; markDirty(); renderSaveState(); });
    element('server-url').addEventListener('input', (event) => { state.settings.metricsApiBaseUrl = event.target.value; markDirty(); renderSaveState(); clearTimeout(serverTimer); serverTimer = setTimeout(() => vscode.postMessage({ type: 'serverChanged', metricsApiBaseUrl: state.settings.metricsApiBaseUrl }), 300); });
    element('save').addEventListener('click', () => { if (state.saving || state.cliError) return; state.saving = true; state.saveError = ''; render(); vscode.postMessage({ type: 'save', settings: state.settings }); });
    window.addEventListener('message', (event) => {
      const message = event.data;
      if (message?.type === 'initial') {
        state.settings = message.initial.settings; state.organization = message.initial.organization; state.cliError = message.initial.cliError || ''; state.dirty = Boolean(message.initial.importedFields?.length); render();
      }
      if (message?.type === 'organization') { state.organization = message.organization; render(); }
      if (message?.type === 'saveResult') { state.saving = false; if (message.ok) { state.settings = message.settings; state.cliError = ''; state.saveError = ''; state.dirty = false; } else { state.saveError = message.error || '保存失败，请稍后重试'; } render(); }
    });
    vscode.postMessage({ type: 'ready' });
  </script>
</body>
</html>`;
}

function getNonce(): string {
  const characters = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  let value = "";
  for (let index = 0; index < 32; index += 1) {
    value += characters.charAt(Math.floor(Math.random() * characters.length));
  }
  return value;
}
