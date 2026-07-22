import * as assert from "assert";
import type * as vscode from "vscode";
import { BlameQueue } from "../blame-queue";

const delay = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
const uri = (value: string) => ({ toString: () => value }) as vscode.Uri;

suite("BlameQueue", () => {
  test("does not exceed the concurrency limit when replacing a running URI", async () => {
    const queue = new BlameQueue<string>();
    const firstUri = uri("file:///tmp/first.ts");
    const secondUri = uri("file:///tmp/second.ts");
    let active = 0;
    let peak = 0;

    const execute = async (value: string) => {
      active += 1;
      peak = Math.max(peak, active);
      await delay(30);
      active -= 1;
      return value;
    };

    const first = queue.enqueue(firstUri, "normal", () => execute("first"));
    await delay(0);
    const replacement = queue.enqueue(firstUri, "normal", () => execute("replacement"));
    const second = queue.enqueue(secondUri, "normal", () => execute("second"));

    await Promise.all([first, replacement, second]);

    assert.ok(peak <= 2, `expected at most 2 concurrent tasks, observed ${peak}`);
    assert.strictEqual(queue.runningCount, 0);
    assert.strictEqual(queue.pendingCount, 0);
  });
});
