"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.getVSCodeApi = getVSCodeApi;
exports.postMessage = postMessage;
let api;
function getVSCodeApi() {
    if (!api) {
        // @ts-expect-error — VS Code injects acquireVsCodeApi globally
        api = acquireVsCodeApi();
    }
    return api;
}
function postMessage(message) {
    getVSCodeApi().postMessage(message);
}
//# sourceMappingURL=vscode.js.map