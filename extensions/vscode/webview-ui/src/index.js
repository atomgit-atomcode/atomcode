"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
const client_1 = require("react-dom/client");
const App_1 = require("./App");
require("./styles/index.css");
const root = document.getElementById('root');
if (root) {
    (0, client_1.createRoot)(root).render(<App_1.App />);
}
//# sourceMappingURL=index.js.map