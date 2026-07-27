import * as assert from "assert";
import * as vscode from "vscode";
import {
  ReportingProfileViewProvider,
  shouldApplySaveResult,
} from "../reporting/reporting-profile-view";
import type {
  InitialReportingState,
  OrganizationLoadResult,
  ReportingProfileService,
} from "../reporting/reporting-profile-service";
import {
  resolveOrganizationOptionsUrl,
  type OrganizationOptions,
  type ReportingSettings,
} from "../reporting/reporting-profile-utils";

const organizationOptions: OrganizationOptions = {
  version: 1,
  departments: [{
    name: "云计算研发部",
    offices: [{ name: "研发四处", teams: ["研发一组"] }],
  }],
};

function settings(metricsApiBaseUrl: string): ReportingSettings {
  return {
    metricsApiBaseUrl,
    profile: {
      departmentName: "云计算研发部",
      officeName: "研发四处",
      teamName: "研发一组",
      userName: "郭立民",
      userEmail: "guolimin.lc@inspur.com",
    },
  };
}

function organizationFor(metricsApiBaseUrl: string): OrganizationLoadResult {
  return {
    endpoint: resolveOrganizationOptionsUrl(metricsApiBaseUrl),
    options: organizationOptions,
    source: "server",
  };
}

interface ServiceStub {
  loadInitialState(): Promise<InitialReportingState>;
  loadOrganizationOptions(rawUrl: string): Promise<OrganizationLoadResult>;
  save(value: ReportingSettings): Promise<ReportingSettings>;
}

function createHarness(service: ServiceStub): {
  posted: unknown[];
  send(message: unknown): Promise<void>;
} {
  let receiveMessage: ((message: unknown) => Promise<void>) | undefined;
  const posted: unknown[] = [];
  const webview = {
    cspSource: "vscode-webview://reporting-profile-test",
    html: "",
    options: {},
    onDidReceiveMessage(listener: (message: unknown) => Promise<void>) {
      receiveMessage = listener;
      return { dispose() {} };
    },
    postMessage(message: unknown) {
      posted.push(message);
      return Promise.resolve(true);
    },
  };
  const view = {
    title: "",
    webview,
    show() {},
  };
  const provider = new ReportingProfileViewProvider(
    service as unknown as ReportingProfileService,
  );
  provider.resolveWebviewView(view as unknown as vscode.WebviewView);

  return {
    posted,
    async send(message: unknown): Promise<void> {
      assert.ok(receiveMessage, "message handler should be registered");
      await receiveMessage(message);
    },
  };
}

suite("Reporting Profile View", () => {
  test("rejects save until organization data belongs to the edited server", async () => {
    const saved = settings("https://old.example.com/prod-api");
    let saveCalls = 0;
    const service: ServiceStub = {
      async loadInitialState() {
        return {
          settings: saved,
          importedFields: [],
          organization: organizationFor(saved.metricsApiBaseUrl),
        };
      },
      async loadOrganizationOptions(rawUrl) {
        return organizationFor(rawUrl);
      },
      async save(value) {
        saveCalls += 1;
        return value;
      },
    };
    const harness = createHarness(service);
    await harness.send({ type: "ready" });
    harness.posted.length = 0;

    await harness.send({
      type: "save",
      settings: settings("https://new.example.com/prod-api"),
      revision: 1,
    });

    assert.strictEqual(saveCalls, 0);
    const result = harness.posted.find((message) =>
      (message as { type?: unknown }).type === "saveResult"
    ) as { ok?: boolean; error?: string; revision?: number } | undefined;
    assert.strictEqual(result?.ok, false);
    assert.match(result?.error ?? "", /组织架构/);
    assert.strictEqual(result?.revision, 1);
  });

  test("locks duplicate saves and echoes the originating revision", async () => {
    const current = settings("https://stats.example.com/prod-api");
    let saveCalls = 0;
    let releaseSave: ((value: ReportingSettings) => void) | undefined;
    let signalStarted: (() => void) | undefined;
    const started = new Promise<void>((resolve) => {
      signalStarted = resolve;
    });
    const pendingSave = new Promise<ReportingSettings>((resolve) => {
      releaseSave = resolve;
    });
    const service: ServiceStub = {
      async loadInitialState() {
        return {
          settings: current,
          importedFields: [],
          organization: organizationFor(current.metricsApiBaseUrl),
        };
      },
      async loadOrganizationOptions(rawUrl) {
        return organizationFor(rawUrl);
      },
      async save() {
        saveCalls += 1;
        signalStarted?.();
        return pendingSave;
      },
    };
    const harness = createHarness(service);
    await harness.send({ type: "ready" });
    harness.posted.length = 0;

    const firstSave = harness.send({ type: "save", settings: current, revision: 7 });
    await started;
    await harness.send({ type: "save", settings: current, revision: 8 });

    assert.strictEqual(saveCalls, 1);
    const duplicate = harness.posted.find((message) =>
      (message as { revision?: unknown }).revision === 8
    ) as { ok?: boolean; error?: string } | undefined;
    assert.strictEqual(duplicate?.ok, false);
    assert.match(duplicate?.error ?? "", /正在保存/);

    releaseSave?.(current);
    await firstSave;
    const success = harness.posted.find((message) =>
      (message as { revision?: unknown }).revision === 7
    ) as { ok?: boolean } | undefined;
    assert.strictEqual(success?.ok, true);
  });

  test("returns organization results with the latest request generation", async () => {
    const initial = settings("https://initial.example.com/prod-api");
    const pending = new Map<string, (result: OrganizationLoadResult) => void>();
    const service: ServiceStub = {
      async loadInitialState() {
        return {
          settings: initial,
          importedFields: [],
          organization: organizationFor(initial.metricsApiBaseUrl),
        };
      },
      loadOrganizationOptions(rawUrl) {
        return new Promise((resolve) => pending.set(rawUrl, resolve));
      },
      async save(value) {
        return value;
      },
    };
    const harness = createHarness(service);
    await harness.send({ type: "ready" });
    harness.posted.length = 0;
    const olderUrl = "https://older.example.com/prod-api";
    const latestUrl = "https://latest.example.com/prod-api";

    const olderRequest = harness.send({
      type: "serverChanged",
      metricsApiBaseUrl: olderUrl,
      generation: 4,
    });
    const latestRequest = harness.send({
      type: "serverChanged",
      metricsApiBaseUrl: latestUrl,
      generation: 5,
    });
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.ok(pending.has(olderUrl));
    assert.ok(pending.has(latestUrl));
    pending.get(olderUrl)?.(organizationFor(olderUrl));
    await olderRequest;
    pending.get(latestUrl)?.(organizationFor(latestUrl));
    await latestRequest;

    const results = harness.posted
      .filter((message) => (message as { type?: unknown }).type === "organization")
      .map((message) => ({
        metricsApiBaseUrl: (message as { metricsApiBaseUrl?: string }).metricsApiBaseUrl,
        generation: (message as { generation?: number }).generation,
      }));
    assert.deepStrictEqual(results, [{
      metricsApiBaseUrl: latestUrl,
      generation: 5,
    }]);
  });

  test("does not apply an old save response over newer edits", () => {
    assert.strictEqual(shouldApplySaveResult(12, 12), true);
    assert.strictEqual(shouldApplySaveResult(13, 12), false);
  });
});
