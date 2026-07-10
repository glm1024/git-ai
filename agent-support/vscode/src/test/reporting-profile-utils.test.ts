import * as assert from "assert";
import {
  mergeReportingSettings,
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
});
