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
      case "retryOrganization":
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
  type: "ready" | "serverChanged" | "retryOrganization" | "save";
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
  if (type === "serverChanged" || type === "retryOrganization") {
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
    main { min-height: 100vh; padding: 18px 16px 80px; }
    h1 { margin: 0; font-size: 18px; font-weight: 600; }
    .description { margin: 6px 0 18px; color: var(--vscode-descriptionForeground); line-height: 1.55; }
    .notice, .status { margin: 0 0 14px; padding: 9px 10px; border-left: 2px solid var(--vscode-textLink-foreground); background: var(--vscode-textBlockQuote-background); color: var(--vscode-descriptionForeground); line-height: 1.5; }
    .notice[hidden], .status[hidden] { display: none; }
    .status.error { border-color: var(--vscode-errorForeground); color: var(--vscode-errorForeground); }
    .status.success { border-color: var(--vscode-testing-iconPassed); color: var(--vscode-testing-iconPassed); }
    section { padding: 16px 0; border-top: 1px solid var(--vscode-sideBarSectionHeader-border); }
    section:first-of-type { border-top: 0; padding-top: 0; }
    h2 { margin: 0 0 12px; font-size: 13px; font-weight: 600; }
    .field { display: grid; gap: 6px; margin: 0 0 14px; }
    .field:last-child { margin-bottom: 0; }
    label { font-weight: 500; }
    input, select { width: 100%; min-height: 28px; border: 1px solid var(--vscode-input-border, transparent); border-radius: 2px; padding: 4px 7px; background: var(--vscode-input-background); color: var(--vscode-input-foreground); font: inherit; }
    input:focus, select:focus, button:focus { outline: 1px solid var(--vscode-focusBorder); outline-offset: 1px; }
    input:disabled, select:disabled { opacity: 0.6; }
    .help { color: var(--vscode-descriptionForeground); font-size: 12px; line-height: 1.45; }
    .save-bar { position: fixed; bottom: 0; left: 0; right: 0; display: flex; justify-content: flex-end; gap: 8px; padding: 12px 16px; border-top: 1px solid var(--vscode-sideBarSectionHeader-border); background: var(--vscode-sideBar-background); }
    button { min-height: 28px; border: 0; border-radius: 2px; padding: 0 12px; color: var(--vscode-button-foreground); background: var(--vscode-button-background); font: inherit; cursor: pointer; }
    button:hover:not(:disabled) { background: var(--vscode-button-hoverBackground); }
    button.secondary { color: var(--vscode-button-secondaryForeground); background: var(--vscode-button-secondaryBackground); }
    button.secondary:hover:not(:disabled) { background: var(--vscode-button-secondaryHoverBackground); }
    button:disabled { opacity: 0.6; cursor: not-allowed; }
    #team-field[hidden] { display: none; }
  </style>
</head>
<body>
  <main>
    <h1>数据上报设置</h1>
    <p class="description">设置仅用于 Git AI 指标上报；组织架构由当前上报服务器提供。</p>
    <div id="notice" class="notice" role="status" aria-live="polite" hidden></div>
    <div id="status" class="status" role="status" aria-live="polite" hidden></div>
    <section>
      <h2>组织信息</h2>
      <div class="field"><label for="department">部门</label><select id="department"><option value="">请选择部门</option></select></div>
      <div class="field"><label for="office">处</label><select id="office"><option value="">请先选择部门</option></select></div>
      <div class="field" id="team-field"><label for="team">组</label><select id="team"><option value="">请选择组</option></select></div>
    </section>
    <section>
      <h2>人员信息</h2>
      <div class="field"><label for="user-name">姓名</label><input id="user-name" autocomplete="name" /></div>
      <div class="field"><label for="user-email">公司邮箱</label><input id="user-email" type="email" inputmode="email" autocomplete="email" /></div>
    </section>
    <section>
      <h2>上报服务器</h2>
      <div class="field"><label for="server-url">上报服务器地址</label><input id="server-url" type="url" inputmode="url" spellcheck="false" /><div class="help">可填写服务器基础地址或带有 /prod-api 的地址。修改后将自动加载该服务器的组织架构。</div></div>
    </section>
  </main>
  <div class="save-bar"><button id="retry" class="secondary" type="button" hidden>重试加载</button><button id="save" type="button">保存</button></div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const state = { settings: { metricsApiBaseUrl: '', profile: { departmentName: '', officeName: '', teamName: '', userName: '', userEmail: '' } }, organization: undefined, cliError: '', saving: false };
    let serverTimer;
    const element = (id) => document.getElementById(id);
    const setStatus = (text, kind = '') => { const node = element('status'); node.textContent = text || ''; node.className = 'status ' + kind; node.hidden = !text; };
    const updateSelect = (id, placeholder, items, selected) => {
      const node = element(id); node.replaceChildren();
      const blank = document.createElement('option'); blank.value = ''; blank.textContent = placeholder; node.append(blank);
      for (const item of items) { const option = document.createElement('option'); option.value = item; option.textContent = item; if (item === selected) option.selected = true; node.append(option); }
      if (selected && !items.includes(selected)) { const invalid = document.createElement('option'); invalid.value = selected; invalid.textContent = selected + '（已失效）'; invalid.selected = true; node.append(invalid); }
    };
    const offices = () => state.organization?.options?.departments.find((department) => department.name === state.settings.profile.departmentName)?.offices || [];
    const teams = () => offices().find((office) => office.name === state.settings.profile.officeName)?.teams || [];
    const render = () => {
      const options = state.organization?.options;
      updateSelect('department', '请选择部门', options?.departments.map((department) => department.name) || [], state.settings.profile.departmentName);
      updateSelect('office', state.settings.profile.departmentName ? '请选择处' : '请先选择部门', offices().map((office) => office.name), state.settings.profile.officeName);
      const availableTeams = teams(); const teamField = element('team-field'); teamField.hidden = availableTeams.length === 0;
      updateSelect('team', '请选择组', availableTeams, state.settings.profile.teamName);
      element('office').disabled = !state.settings.profile.departmentName || !options;
      element('team').disabled = !state.settings.profile.officeName || !options;
      element('user-name').value = state.settings.profile.userName;
      element('user-email').value = state.settings.profile.userEmail;
      element('server-url').value = state.settings.metricsApiBaseUrl;
      const retry = element('retry'); retry.hidden = !state.organization?.error; retry.disabled = state.saving;
      element('save').disabled = state.saving || Boolean(state.cliError);
      element('save').textContent = state.saving ? '保存中…' : '保存';
    };
    const showOrganizationStatus = () => {
      if (!state.organization) return;
      if (state.organization.error) setStatus(state.organization.error, 'error');
      else if (state.organization.source === 'cache') setStatus('正在使用上次成功加载的组织架构', '');
      else setStatus('', '');
    };
    element('department').addEventListener('change', (event) => { state.settings.profile.departmentName = event.target.value; state.settings.profile.officeName = ''; state.settings.profile.teamName = ''; render(); });
    element('office').addEventListener('change', (event) => { state.settings.profile.officeName = event.target.value; state.settings.profile.teamName = ''; render(); });
    element('team').addEventListener('change', (event) => { state.settings.profile.teamName = event.target.value; });
    element('user-name').addEventListener('input', (event) => { state.settings.profile.userName = event.target.value; });
    element('user-email').addEventListener('input', (event) => { state.settings.profile.userEmail = event.target.value; });
    element('server-url').addEventListener('input', (event) => { state.settings.metricsApiBaseUrl = event.target.value; clearTimeout(serverTimer); serverTimer = setTimeout(() => vscode.postMessage({ type: 'serverChanged', metricsApiBaseUrl: state.settings.metricsApiBaseUrl }), 300); });
    element('retry').addEventListener('click', () => vscode.postMessage({ type: 'retryOrganization', metricsApiBaseUrl: state.settings.metricsApiBaseUrl }));
    element('save').addEventListener('click', () => { if (state.saving || state.cliError) return; state.saving = true; render(); vscode.postMessage({ type: 'save', settings: state.settings }); });
    window.addEventListener('message', (event) => {
      const message = event.data;
      if (message?.type === 'initial') {
        state.settings = message.initial.settings; state.organization = message.initial.organization; state.cliError = message.initial.cliError || '';
        const imported = message.initial.importedFields || []; const notice = element('notice'); notice.textContent = imported.length ? '已从 Kilo 补齐：' + imported.join('、') + '。保存后用于 Git AI 指标上报。' : ''; notice.hidden = !imported.length;
        if (state.cliError) setStatus(state.cliError, 'error'); else showOrganizationStatus(); render();
      }
      if (message?.type === 'organization') { state.organization = message.organization; showOrganizationStatus(); render(); }
      if (message?.type === 'saveResult') { state.saving = false; if (message.ok) { state.settings = message.settings; state.cliError = ''; setStatus('保存成功，后续 Git AI 指标将使用此配置。', 'success'); } else { setStatus(message.error || '保存失败，请稍后重试', 'error'); } render(); }
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
