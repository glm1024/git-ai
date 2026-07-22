export const DEFAULT_METRICS_API_BASE_URL = "http://100.7.132.102:8081/prod-api";
export const ORGANIZATION_UNAVAILABLE_MESSAGE = "上报地址不可用";

const ORGANIZATION_OPTIONS_PATH = "/api/v1/ai-code-stats/organization-options";
const KNOWN_ENDPOINT_PATHS = [
  "/worker/metrics/upload",
  ORGANIZATION_OPTIONS_PATH,
  "/api/v1/ingest/ai-code-stats",
  "/api/v1/ingest/ai-token-usage",
];

export interface ReportingProfile {
  departmentName: string;
  officeName: string;
  teamName: string;
  userName: string;
  userEmail: string;
}

export interface ReportingSettings {
  metricsApiBaseUrl: string;
  profile: ReportingProfile;
}

export interface OrganizationOffice {
  name: string;
  teams: string[];
}

export interface OrganizationDepartment {
  name: string;
  offices: OrganizationOffice[];
}

export interface OrganizationOptions {
  version?: number;
  departments: OrganizationDepartment[];
}

export function emptyReportingProfile(): ReportingProfile {
  return { departmentName: "", officeName: "", teamName: "", userName: "", userEmail: "" };
}

export function normalizeReportingSettings(settings: ReportingSettings): ReportingSettings {
  return {
    metricsApiBaseUrl: normalizeMetricsApiBaseUrl(settings.metricsApiBaseUrl),
    profile: {
      departmentName: settings.profile.departmentName.trim(),
      officeName: settings.profile.officeName.trim(),
      teamName: settings.profile.teamName.trim(),
      userName: settings.profile.userName.trim(),
      userEmail: settings.profile.userEmail.trim().toLowerCase(),
    },
  };
}

export function normalizeReportingSettingsForOrganization(
  settings: ReportingSettings,
  organizationOptions?: OrganizationOptions,
): ReportingSettings {
  const normalized = normalizeReportingSettings(settings);
  if (!organizationOptions || !normalized.profile.teamName) {
    return normalized;
  }
  const office = organizationOptions.departments
    .find((department) => department.name === normalized.profile.departmentName)?.offices
    .find((item) => item.name === normalized.profile.officeName);
  if (!office || office.teams.length > 0) {
    return normalized;
  }
  return {
    ...normalized,
    profile: { ...normalized.profile, teamName: "" },
  };
}

export function mergeReportingSettings(
  saved: ReportingSettings,
  kilo: ReportingSettings,
  defaultMetricsApiBaseUrl = DEFAULT_METRICS_API_BASE_URL,
): ReportingSettings {
  // A malformed legacy value must not prevent the user from opening this page
  // and correcting it. Git AI's value is preserved for that purpose; an
  // invalid Kilo address is ignored because it is only an optional import.
  const normalizedSaved = normalizeSettingsForMerge(saved, true);
  const normalizedKilo = normalizeSettingsForMerge(kilo, false);
  const pick = (current: string, imported: string) => current || imported;
  return {
    metricsApiBaseUrl: pick(normalizedSaved.metricsApiBaseUrl, normalizedKilo.metricsApiBaseUrl || defaultMetricsApiBaseUrl),
    profile: {
      departmentName: pick(normalizedSaved.profile.departmentName, normalizedKilo.profile.departmentName),
      officeName: pick(normalizedSaved.profile.officeName, normalizedKilo.profile.officeName),
      teamName: pick(normalizedSaved.profile.teamName, normalizedKilo.profile.teamName),
      userName: pick(normalizedSaved.profile.userName, normalizedKilo.profile.userName),
      userEmail: pick(normalizedSaved.profile.userEmail, normalizedKilo.profile.userEmail),
    },
  };
}

function normalizeSettingsForMerge(settings: ReportingSettings, preserveInvalidAddress: boolean): ReportingSettings {
  let metricsApiBaseUrl = settings.metricsApiBaseUrl.trim();
  try {
    metricsApiBaseUrl = normalizeMetricsApiBaseUrl(metricsApiBaseUrl);
  } catch {
    metricsApiBaseUrl = preserveInvalidAddress ? metricsApiBaseUrl : "";
  }
  return {
    metricsApiBaseUrl,
    profile: {
      departmentName: settings.profile.departmentName.trim(),
      officeName: settings.profile.officeName.trim(),
      teamName: settings.profile.teamName.trim(),
      userName: settings.profile.userName.trim(),
      userEmail: settings.profile.userEmail.trim().toLowerCase(),
    },
  };
}

export function normalizeMetricsApiBaseUrl(rawValue: string): string {
  const trimmed = rawValue.trim();
  if (!trimmed) {
    return "";
  }
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    throw new Error("上报服务器地址格式不正确，请填写以 http:// 或 https:// 开头的地址");
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error("上报服务器地址必须以 http:// 或 https:// 开头");
  }
  if (url.username || url.password || url.search || url.hash) {
    throw new Error("上报服务器地址不能包含账号、查询参数或片段");
  }
  let pathname = url.pathname.replace(/\/+$/, "") || "/";
  for (const endpointPath of KNOWN_ENDPOINT_PATHS) {
    if (pathname === endpointPath) {
      pathname = "/";
      break;
    }
    if (pathname.endsWith(endpointPath)) {
      pathname = pathname.slice(0, -endpointPath.length) || "/";
      break;
    }
  }
  url.pathname = pathname;
  return url.toString().replace(/\/$/, "");
}

export function resolveOrganizationOptionsUrl(rawValue: string): string {
  const baseUrl = normalizeMetricsApiBaseUrl(rawValue);
  if (!baseUrl) {
    throw new Error("请先填写上报服务器地址");
  }
  const url = new URL(baseUrl);
  const basePath = url.pathname.replace(/\/+$/, "");
  url.pathname = `${basePath}${ORGANIZATION_OPTIONS_PATH}` || ORGANIZATION_OPTIONS_PATH;
  return url.toString();
}

export function normalizeOrganizationOptions(value: unknown): OrganizationOptions {
  if (!value || typeof value !== "object") {
    throw new Error("组织架构响应格式无效");
  }
  const record = value as { version?: unknown; departments?: unknown };
  if (!Array.isArray(record.departments)) {
    throw new Error("组织架构响应缺少部门列表");
  }
  const departments: OrganizationDepartment[] = [];
  for (const department of record.departments) {
    if (!department || typeof department !== "object") {
      continue;
    }
    const departmentRecord = department as { name?: unknown; offices?: unknown };
    const name = typeof departmentRecord.name === "string" ? departmentRecord.name.trim() : "";
    if (!name || !Array.isArray(departmentRecord.offices)) {
      continue;
    }
    const offices: OrganizationOffice[] = [];
    for (const office of departmentRecord.offices) {
      if (!office || typeof office !== "object") {
        continue;
      }
      const officeRecord = office as { name?: unknown; teams?: unknown };
      const officeName = typeof officeRecord.name === "string" ? officeRecord.name.trim() : "";
      if (!officeName) {
        continue;
      }
      const teams = Array.isArray(officeRecord.teams)
        ? [...new Set(officeRecord.teams.filter((team): team is string => typeof team === "string").map((team) => team.trim()).filter(Boolean))]
        : [];
      offices.push({ name: officeName, teams });
    }
    if (offices.length > 0) {
      departments.push({ name, offices });
    }
  }
  if (!departments.length) {
    throw new Error("上报服务器未返回可用组织架构");
  }
  return {
    version: typeof record.version === "number" ? record.version : undefined,
    departments,
  };
}

export function formatOrganizationHttpError(status: number): string {
  if (status === 401 || status === 403) {
    return `无权访问组织架构服务（HTTP ${status}），请联系管理员检查服务权限`;
  }
  if (status === 404) {
    return "找不到组织架构接口（HTTP 404），请检查上报服务器地址或联系管理员确认服务版本";
  }
  if (status === 429) {
    return "组织架构服务请求过于频繁（HTTP 429），请稍后重试";
  }
  if (status >= 500) {
    return `组织架构服务暂时不可用（HTTP ${status}），请稍后重试或联系管理员`;
  }
  return `组织架构服务请求失败（HTTP ${status}），请检查上报服务器地址和服务配置`;
}

export function formatOrganizationRequestError(error: unknown): string {
  const name = objectStringField(error, "name");
  const code = nestedErrorCode(error);
  if (name === "AbortError" || ["ABORT_ERR", "ETIMEDOUT", "UND_ERR_CONNECT_TIMEOUT"].includes(code)) {
    return "连接上报服务器超时，请检查服务器地址和网络后重试";
  }
  if (code === "ENOTFOUND") {
    return "无法解析上报服务器地址，请检查域名和网络后重试";
  }
  if (code === "ECONNREFUSED") {
    return "无法连接上报服务器，请检查服务器地址、端口以及服务是否已启动";
  }
  if (["EHOSTUNREACH", "ENETUNREACH", "ECONNRESET", "UND_ERR_SOCKET"].includes(code)) {
    return "无法连接上报服务器，请检查服务器地址和网络后重试";
  }
  if (["DEPTH_ZERO_SELF_SIGNED_CERT", "CERT_HAS_EXPIRED", "UNABLE_TO_VERIFY_LEAF_SIGNATURE"].includes(code)) {
    return "无法建立安全连接，请联系管理员检查上报服务器的 HTTPS 证书";
  }
  const message = error instanceof Error ? error.message.trim() : "";
  if (error instanceof SyntaxError) {
    return "组织架构服务返回的数据格式不正确，请联系管理员检查服务配置";
  }
  if (/^(fetch failed|failed to fetch)$/i.test(message)) {
    return "无法连接上报服务器，请检查服务器地址、端口和网络后重试";
  }
  if (/[\u3400-\u9fff]/.test(message)) {
    return message;
  }
  return "无法加载组织架构，请检查服务器地址和网络后重试";
}

export function validateReportingSettings(settings: ReportingSettings, organizationOptions?: OrganizationOptions): string | undefined {
  let normalized: ReportingSettings;
  try {
    normalized = normalizeReportingSettings(settings);
  } catch {
    return ORGANIZATION_UNAVAILABLE_MESSAGE;
  }
  if (!normalized.metricsApiBaseUrl) {
    return ORGANIZATION_UNAVAILABLE_MESSAGE;
  }
  if (!normalized.profile.departmentName) {
    return "请选择部门";
  }
  if (!normalized.profile.officeName) {
    return "请选择处";
  }
  if (!normalized.profile.userName) {
    return "请填写姓名";
  }
  if (!isValidEmail(normalized.profile.userEmail)) {
    return "请填写有效的公司邮箱";
  }
  if (!organizationOptions) {
    return undefined;
  }
  const department = organizationOptions.departments.find((item) => item.name === normalized.profile.departmentName);
  if (!department) {
    return "当前部门已失效，请重新选择";
  }
  const office = department.offices.find((item) => item.name === normalized.profile.officeName);
  if (!office) {
    return "当前处已失效，请重新选择";
  }
  if (office.teams.length > 0 && !normalized.profile.teamName) {
    return "请选择组";
  }
  if (office.teams.length > 0 && normalized.profile.teamName && !office.teams.includes(normalized.profile.teamName)) {
    return "当前组已失效，请重新选择";
  }
  return undefined;
}

export function officeOptions(settings: ReportingSettings, options?: OrganizationOptions): OrganizationOffice[] {
  return options?.departments.find((department) => department.name === settings.profile.departmentName)?.offices ?? [];
}

export function teamOptions(settings: ReportingSettings, options?: OrganizationOptions): string[] {
  return officeOptions(settings, options).find((office) => office.name === settings.profile.officeName)?.teams ?? [];
}

function isValidEmail(value: string): boolean {
  const at = value.indexOf("@");
  return at > 0 && at < value.length - 3 && value.slice(at + 1).includes(".");
}

function nestedErrorCode(value: unknown, depth = 0): string {
  if (!value || typeof value !== "object" || depth > 3) {
    return "";
  }
  const code = objectStringField(value, "code");
  if (code) {
    return code;
  }
  return nestedErrorCode((value as { cause?: unknown }).cause, depth + 1);
}

function objectStringField(value: unknown, key: string): string {
  if (!value || typeof value !== "object") {
    return "";
  }
  const field = (value as Record<string, unknown>)[key];
  return typeof field === "string" ? field : "";
}
