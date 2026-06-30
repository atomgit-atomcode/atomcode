package com.atomcode.jetbrains.ui.message;

import com.atomcode.jetbrains.ui.jcef.JBCefQueryHandlers;
import com.intellij.ui.jcef.JBCefApp;
import com.intellij.ui.jcef.JBCefBrowser;
import org.cef.browser.CefBrowser;
import org.cef.browser.CefFrame;
import org.cef.handler.CefLoadHandlerAdapter;

import javax.swing.JComponent;
import java.util.function.Consumer;

/**
 * Keeps JBCefJSQuery handler registration out of JBCefMessageView.
 *
 * IntelliJ 2026.2 no longer exposes every nested type used by older
 * JBCefJSQuery generic signatures. Registering the handler from Java with a raw
 * Function avoids writing JBCefJSQuery.Response into our component bytecode,
 * which AWT reflects while constructing JPanel subclasses.
 */
final class JBCefMessageBridge {
    private final JBCefBrowser browser;
    private final Object query;

    static boolean isSupported() {
        return JBCefApp.isSupported();
    }

    JBCefMessageBridge(Consumer<String> onHostMessage, Runnable onReady) throws ReflectiveOperationException {
        browser = new JBCefBrowser();
        query = JBCefQueryHandlers.create(browser, message -> {
            if ("js:ready".equals(message)) {
                onReady.run();
            } else {
                onHostMessage.accept(message);
            }
        });

        browser.getJBCefClient().addLoadHandler(new CefLoadHandlerAdapter() {
            @Override
            public void onLoadEnd(CefBrowser cefBrowser, CefFrame frame, int httpStatusCode) {
                if (frame != null && frame.isMain()) {
                    try {
                        cefBrowser.executeJavaScript(
                            "window.atomcodeHost = function(msg) { " + JBCefQueryHandlers.inject(query, "msg") + " }",
                            cefBrowser.getURL(),
                            0
                        );
                        onReady.run();
                    } catch (ReflectiveOperationException ignored) {
                    }
                }
            }
        }, browser.getCefBrowser());
    }

    JComponent getComponent() {
        return browser.getComponent();
    }

    void loadHtml(String html) {
        browser.loadHTML(html);
    }

    void executeJavaScript(String code) {
        try {
            browser.getCefBrowser().executeJavaScript(code, browser.getCefBrowser().getURL(), 0);
        } catch (Exception ignored) {
        }
    }

    void dispose() {
        try {
            JBCefQueryHandlers.dispose(query);
        } catch (ReflectiveOperationException ignored) {
        }
        browser.dispose();
    }
}
