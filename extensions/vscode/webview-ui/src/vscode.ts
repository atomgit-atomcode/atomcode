interface VSCodeApi {
  postMessage(message: unknown): void;
  getState(): unknown;
  setState(state: unknown): void;
}

let api: VSCodeApi | undefined;

export function getVSCodeApi(): VSCodeApi {
  if (!api) {
    // @ts-expect-error — VS Code injects acquireVsCodeApi globally
    api = acquireVsCodeApi();
  }
  return api!;
}

export function postMessage(message: unknown): void {
  getVSCodeApi().postMessage(message);
}
