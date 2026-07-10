import { spawn } from "child_process";
import * as vscode from "vscode";
import { getGitAiBinary, resolveGitAiBinary } from "../utils/binary-path";
import {
  DEFAULT_METRICS_API_BASE_URL,
  emptyReportingProfile,
  mergeReportingSettings,
  normalizeOrganizationOptions,
  normalizeReportingSettings,
  resolveOrganizationOptionsUrl,
  type OrganizationOptions,
  type ReportingProfile,
  type ReportingSettings,
} from "./reporting-profile-utils";

const ORGANIZATION_CACHE_KEY = "git-ai.reporting.organization-options";
const ORGANIZATION_CACHE_MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000;
const ORGANIZATION_REQUEST_TIMEOUT_MS = 8_000;
const CLI_TIMEOUT_MS = 10_000;
const MAX_CLI_OUTPUT_BYTES = 1_000_000;

interface OrganizationCacheEntry {
  fetchedAt: number;
  options: OrganizationOptions;
}

interface OrganizationCache {
  [endpoint: string]: OrganizationCacheEntry;
}

export interface OrganizationLoadResult {
  endpoint?: string;
  options?: OrganizationOptions;
  source?: "server" | "cache";
  fetchedAt?: number;
  error?: string;
}

export interface InitialReportingState {
  settings: ReportingSettings;
  importedFields: string[];
  organization: OrganizationLoadResult;
  cliError?: string;
}

export class ReportingProfileService {
  public constructor(private readonly context: vscode.ExtensionContext) {}

  public async loadInitialState(): Promise<InitialReportingState> {
    const kiloSettings = this.readKiloSettings();
    let savedSettings: ReportingSettings = {
      metricsApiBaseUrl: "",
      profile: emptyReportingProfile(),
    };
    let cliError: string | undefined;
    try {
      savedSettings = await this.readGitAiSettings();
    } catch (error) {
      cliError = toErrorMessage(error, "无法读取 Git AI CLI 配置");
    }

    const settings = mergeReportingSettings(savedSettings, kiloSettings, DEFAULT_METRICS_API_BASE_URL);
    const importedFields = importedMissingFields(savedSettings, kiloSettings);
    return {
      settings,
      importedFields,
      organization: await this.loadOrganizationOptions(settings.metricsApiBaseUrl),
      cliError,
    };
  }

  public async loadOrganizationOptions(rawUrl: string): Promise<OrganizationLoadResult> {
    let endpoint: string;
    try {
      endpoint = resolveOrganizationOptionsUrl(rawUrl);
    } catch (error) {
      return { error: toErrorMessage(error, "上报服务器地址无效") };
    }

    try {
      const controller = new AbortController();
      const timeout = setTimeout(() => controller.abort(), ORGANIZATION_REQUEST_TIMEOUT_MS);
      let response: Response;
      try {
        response = await fetch(endpoint, { signal: controller.signal, headers: { Accept: "application/json" } });
      } finally {
        clearTimeout(timeout);
      }
      if (!response.ok) {
        throw new Error(`组织架构服务返回 HTTP ${response.status}`);
      }
      const options = normalizeOrganizationOptions(await response.json());
      const fetchedAt = Date.now();
      const cache = this.context.globalState.get<OrganizationCache>(ORGANIZATION_CACHE_KEY) ?? {};
      cache[endpoint] = { fetchedAt, options };
      await this.context.globalState.update(ORGANIZATION_CACHE_KEY, cache);
      return { endpoint, options, source: "server", fetchedAt };
    } catch (error) {
      const cache = this.context.globalState.get<OrganizationCache>(ORGANIZATION_CACHE_KEY) ?? {};
      const cached = cache[endpoint];
      if (cached && Date.now() - cached.fetchedAt <= ORGANIZATION_CACHE_MAX_AGE_MS) {
        return {
          endpoint,
          options: cached.options,
          source: "cache",
          fetchedAt: cached.fetchedAt,
          error: `${toErrorMessage(error, "组织架构服务不可用")}；正在使用上次成功加载的数据`,
        };
      }
      return { endpoint, error: toErrorMessage(error, "无法加载组织架构") };
    }
  }

  public async save(settings: ReportingSettings): Promise<ReportingSettings> {
    const normalized = normalizeReportingSettings(settings);
    const payload = JSON.stringify({
      metrics_api_base_url: normalized.metricsApiBaseUrl,
      reporting_profile: toCliProfile(normalized.profile),
    });
    await this.runGitAi(["config", "reporting-profile", "set", "--stdin"], payload);
    return this.readGitAiSettings();
  }

  private readKiloSettings(): ReportingSettings {
    const configuration = vscode.workspace.getConfiguration("kilo-code");
    const get = (key: string): string => {
      const value = configuration.get<unknown>(key);
      return typeof value === "string" ? value : "";
    };
    return {
      metricsApiBaseUrl: get("aiCodeStatsWebhookUrl"),
      profile: {
        departmentName: get("aiCodeStatsDepartmentName"),
        officeName: get("aiCodeStatsOfficeName"),
        teamName: get("aiCodeStatsTeamName"),
        userName: get("aiCodeStatsUserName"),
        userEmail: get("aiCodeStatsUserEmail"),
      },
    };
  }

  private async readGitAiSettings(): Promise<ReportingSettings> {
    const output = await this.runGitAi(["config", "reporting-profile"]);
    const raw = JSON.parse(output) as { metrics_api_base_url?: unknown; reporting_profile?: unknown };
    const rawProfile = raw.reporting_profile && typeof raw.reporting_profile === "object"
      ? raw.reporting_profile as Record<string, unknown>
      : {};
    return normalizeReportingSettings({
      metricsApiBaseUrl: typeof raw.metrics_api_base_url === "string" ? raw.metrics_api_base_url : "",
      profile: {
        departmentName: stringField(rawProfile.department_name),
        officeName: stringField(rawProfile.office_name),
        teamName: stringField(rawProfile.team_name),
        userName: stringField(rawProfile.user_name),
        userEmail: stringField(rawProfile.user_email),
      },
    });
  }

  private async runGitAi(args: string[], input?: string): Promise<string> {
    await resolveGitAiBinary();
    return new Promise((resolve, reject) => {
      const child = spawn(getGitAiBinary(), args, {
        windowsHide: true,
        stdio: ["pipe", "pipe", "pipe"],
      });
      let stdout = "";
      let stderr = "";
      let outputBytes = 0;
      const timeout = setTimeout(() => {
        child.kill();
        reject(new Error("Git AI CLI 响应超时，请确认 CLI 已安装并重启 VS Code 后重试"));
      }, CLI_TIMEOUT_MS);
      const collect = (target: "stdout" | "stderr", chunk: Buffer) => {
        outputBytes += chunk.length;
        if (outputBytes > MAX_CLI_OUTPUT_BYTES) {
          child.kill();
          reject(new Error("Git AI CLI 输出过大，已中止本次操作"));
          return;
        }
        if (target === "stdout") {
          stdout += chunk.toString("utf8");
        } else {
          stderr += chunk.toString("utf8");
        }
      };
      child.stdout.on("data", (chunk: Buffer) => collect("stdout", chunk));
      child.stderr.on("data", (chunk: Buffer) => collect("stderr", chunk));
      child.on("error", (error) => {
        clearTimeout(timeout);
        reject(new Error(`无法启动 Git AI CLI：${error.message}`));
      });
      child.on("close", (code) => {
        clearTimeout(timeout);
        if (code === 0) {
          resolve(stdout.trim());
        } else {
          reject(new Error((stderr || stdout || `Git AI CLI 退出码 ${code}`).trim()));
        }
      });
      if (input !== undefined) {
        child.stdin.write(input);
      }
      child.stdin.end();
    });
  }
}

function importedMissingFields(saved: ReportingSettings, kilo: ReportingSettings): string[] {
  const fields: Array<[string, string, string]> = [
    ["上报服务器地址", saved.metricsApiBaseUrl, kilo.metricsApiBaseUrl],
    ["部门", saved.profile.departmentName, kilo.profile.departmentName],
    ["处", saved.profile.officeName, kilo.profile.officeName],
    ["组", saved.profile.teamName, kilo.profile.teamName],
    ["姓名", saved.profile.userName, kilo.profile.userName],
    ["公司邮箱", saved.profile.userEmail, kilo.profile.userEmail],
  ];
  return fields.filter(([, current, imported]) => !current.trim() && Boolean(imported.trim())).map(([label]) => label);
}

function toCliProfile(profile: ReportingProfile): Record<string, string | undefined> {
  return {
    department_name: profile.departmentName,
    office_name: profile.officeName,
    team_name: profile.teamName || undefined,
    user_name: profile.userName,
    user_email: profile.userEmail,
  };
}

function stringField(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function toErrorMessage(error: unknown, fallback: string): string {
  return error instanceof Error && error.message ? error.message : fallback;
}
