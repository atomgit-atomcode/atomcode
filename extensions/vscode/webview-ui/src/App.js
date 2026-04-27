"use strict";
var __importDefault = (this && this.__importDefault) || function (mod) {
    return (mod && mod.__esModule) ? mod : { "default": mod };
};
Object.defineProperty(exports, "__esModule", { value: true });
exports.App = App;
const react_1 = __importDefault(require("react"));
const ChatProvider_1 = require("./state/ChatProvider");
function App() {
    return (<ChatProvider_1.ChatProvider>
      <div className="app">AtomCode React Webview Loading...</div>
    </ChatProvider_1.ChatProvider>);
}
//# sourceMappingURL=App.js.map