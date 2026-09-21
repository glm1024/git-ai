import * as assert from "assert";
import { HIDDEN_WINDOWS_PROCESS_OPTIONS } from "../utils/process-options";

suite("Process options", () => {
  test("hide child console windows on Windows", () => {
    assert.strictEqual(HIDDEN_WINDOWS_PROCESS_OPTIONS.windowsHide, true);
  });
});
