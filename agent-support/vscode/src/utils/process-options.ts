/**
 * VS Code's extension host is a GUI process on Windows. Console applications
 * spawned without this option can create a transient terminal window and steal
 * focus from the editor.
 */
export const HIDDEN_WINDOWS_PROCESS_OPTIONS = Object.freeze({
  windowsHide: true,
});
