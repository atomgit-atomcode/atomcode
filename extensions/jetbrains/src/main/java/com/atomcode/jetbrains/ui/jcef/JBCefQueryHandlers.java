package com.atomcode.jetbrains.ui.jcef;

import com.intellij.ui.jcef.JBCefBrowserBase;
import com.intellij.ui.jcef.JBCefJSQuery;

import java.util.function.Consumer;
import java.util.function.Function;

/**
 * Registers JBCefJSQuery handlers without leaking unstable nested response
 * types into Kotlin-generated lambda signatures.
 */
public final class JBCefQueryHandlers {
    private JBCefQueryHandlers() {
    }

    public static JBCefJSQuery create(JBCefBrowserBase browser, Consumer<String> handler) {
        JBCefJSQuery query = JBCefJSQuery.create(browser);
        addHandler(query, handler);
        return query;
    }

    @SuppressWarnings({"rawtypes", "unchecked"})
    public static void addHandler(JBCefJSQuery query, Consumer<String> handler) {
        Function rawHandler = (Function<String, Object>) message -> {
            handler.accept(message);
            return null;
        };
        query.addHandler(rawHandler);
    }
}
