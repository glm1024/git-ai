import * as assert from "assert";
import {
  formatOrganizationHttpError,
  formatOrganizationRequestError,
  mergeReportingSettings,
  normalizeReportingSettingsForOrganization,
  ORGANIZATION_UNAVAILABLE_MESSAGE,
  resolveOrganizationOptionsUrl,
  validateReportingSettings,
  type OrganizationOptions,
} from "../reporting/reporting-profile-utils";

suite("Reporting Profile Utilities", () => {
  const organizationOptions: OrganizationOptions = {
    version: 1,
    departments: [{
      name: "云计算研发部",
      offices: [{ name: "研发四处", teams: ["研发一组"] }, { name: "经理室", teams: [] }],
    }],
  };

  test("preserves Git AI values and only fills missing fields from Kilo", () => {
    const merged = mergeReportingSettings(
      {
        metricsApiBaseUrl: "https://git-ai.example.com/prod-api",
        profile: { departmentName: "云计算研发部", officeName: "", teamName: "", userName: "", userEmail: "" },
      },
      {
        metricsApiBaseUrl: "https://kilo.example.com/prod-api",
        profile: { departmentName: "旧部门", officeName: "研发四处", teamName: "研发一组", userName: "郭立民", userEmail: "GUO@INSPUR.COM" },
      },
      "http://default.example.com/prod-api",
    );

    assert.strictEqual(merged.metricsApiBaseUrl, "https://git-ai.example.com/prod-api");
    assert.strictEqual(merged.profile.departmentName, "云计算研发部");
    assert.strictEqual(merged.profile.officeName, "研发四处");
    assert.strictEqual(merged.profile.teamName, "研发一组");
    assert.strictEqual(merged.profile.userName, "郭立民");
    assert.strictEqual(merged.profile.userEmail, "guo@inspur.com");
  });

  test("derives organization options URL while preserving a path prefix", () => {
    assert.strictEqual(
      resolveOrganizationOptionsUrl("http://stats.example.com/prod-api/worker/metrics/upload"),
      "http://stats.example.com/prod-api/api/v1/ai-code-stats/organization-options",
    );
  });

  test("uses a Chinese validation message for a malformed server address", () => {
    assert.throws(
      () => resolveOrganizationOptionsUrl("not-a-url"),
      /上报服务器地址格式不正确/,
    );
  });

  test("keeps an invalid saved address editable and ignores an invalid Kilo address", () => {
    const merged = mergeReportingSettings(
      {
        metricsApiBaseUrl: "ftp://legacy.example.com",
        profile: { departmentName: "", officeName: "", teamName: "", userName: "", userEmail: "" },
      },
      {
        metricsApiBaseUrl: "not-a-url",
        profile: { departmentName: "云计算研发部", officeName: "研发四处", teamName: "", userName: "郭立民", userEmail: "GUO@INSPUR.COM" },
      },
    );

    assert.strictEqual(merged.metricsApiBaseUrl, "ftp://legacy.example.com");
    assert.strictEqual(merged.profile.userEmail, "guo@inspur.com");
  });

  test("uses one short user-facing message for missing and malformed reporting addresses", () => {
    const profile = {
      departmentName: "云计算研发部",
      officeName: "研发四处",
      teamName: "研发一组",
      userName: "郭立民",
      userEmail: "guo@inspur.com",
    };

    assert.strictEqual(
      validateReportingSettings({ metricsApiBaseUrl: "", profile }),
      ORGANIZATION_UNAVAILABLE_MESSAGE,
    );
    assert.strictEqual(
      validateReportingSettings({ metricsApiBaseUrl: "not-a-url", profile }),
      ORGANIZATION_UNAVAILABLE_MESSAGE,
    );
  });

  test("requires a valid cascading organization selection before save", () => {
    const error = validateReportingSettings({
      metricsApiBaseUrl: "http://stats.example.com/prod-api",
      profile: {
        departmentName: "云计算研发部",
        officeName: "研发四处",
        teamName: "",
        userName: "郭立民",
        userEmail: "guo@inspur.com",
      },
    }, organizationOptions);

    assert.strictEqual(error, "请选择组");
  });

  test("ignores a stale saved team when the selected office has no teams", () => {
    const error = validateReportingSettings({
      metricsApiBaseUrl: "http://stats.example.com/prod-api",
      profile: {
        departmentName: "云计算研发部",
        officeName: "经理室",
        teamName: "研发一组",
        userName: "郭立民",
        userEmail: "guo@inspur.com",
      },
    }, organizationOptions);

    assert.strictEqual(error, undefined);
  });

  test("preserves the saved team while organization options are unavailable", () => {
    const normalized = normalizeReportingSettingsForOrganization({
      metricsApiBaseUrl: "http://stats.example.com/prod-api",
      profile: {
        departmentName: "云计算研发部",
        officeName: "研发四处",
        teamName: "研发一组",
        userName: "郭立民",
        userEmail: "guo@inspur.com",
      },
    });

    assert.strictEqual(normalized.profile.teamName, "研发一组");
  });

  test("removes a stale team only when a successful organization response confirms the office has no teams", () => {
    const normalized = normalizeReportingSettingsForOrganization({
      metricsApiBaseUrl: "http://stats.example.com/prod-api",
      profile: {
        departmentName: "云计算研发部",
        officeName: "经理室",
        teamName: "研发一组",
        userName: "郭立民",
        userEmail: "guo@inspur.com",
      },
    }, organizationOptions);

    assert.strictEqual(normalized.profile.teamName, "");
  });

  test("converts fetch failures and HTTP errors into actionable Chinese messages", () => {
    const connectionError = new TypeError("fetch failed") as TypeError & { cause?: { code: string } };
    connectionError.cause = { code: "ECONNREFUSED" };

    assert.strictEqual(
      formatOrganizationRequestError(connectionError),
      "无法连接上报服务器，请检查服务器地址、端口以及服务是否已启动",
    );
    assert.strictEqual(
      formatOrganizationRequestError(new TypeError("fetch failed")),
      "无法连接上报服务器，请检查服务器地址、端口和网络后重试",
    );
    assert.strictEqual(
      formatOrganizationHttpError(404),
      "找不到组织架构接口（HTTP 404），请检查上报服务器地址或联系管理员确认服务版本",
    );
  });
});
