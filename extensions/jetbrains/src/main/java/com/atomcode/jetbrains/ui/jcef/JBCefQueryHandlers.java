package com.atomcode.jetbrains.ui.jcef;

import java.lang.reflect.Method;
import java.util.function.Consumer;
import java.util.function.Function;

/**
 * Registers JBCefJSQuery handlers without leaking unstable nested response
 * types into Kotlin-generated lambda signatures.
 */
public final class JBCefQueryHandlers {
    private JBCefQueryHandlers() {
    }

    public static Object create(Object browser, Consumer<String> handler) throws ReflectiveOperationException {
        ClassLoader loader = browser.getClass().getClassLoader();
        Class<?> queryClass = Class.forName("com.intellij.ui.jcef.JBCefJSQuery", true, loader);
        Method create = findCreateMethod(queryClass, browser.getClass());
        Object query = create.invoke(null, browser);
        addHandler(query, handler);
        return query;
    }

    @SuppressWarnings({"rawtypes", "unchecked"})
    public static void addHandler(Object query, Consumer<String> handler) throws ReflectiveOperationException {
        Function rawHandler = (Function<String, Object>) message -> {
            handler.accept(message);
            return null;
        };
        query.getClass().getMethod("addHandler", Function.class).invoke(query, rawHandler);
    }

    public static String inject(Object query, String argumentExpression) throws ReflectiveOperationException {
        return (String) query.getClass().getMethod("inject", String.class).invoke(query, argumentExpression);
    }

    public static void dispose(Object query) throws ReflectiveOperationException {
        query.getClass().getMethod("dispose").invoke(query);
    }

    private static Method findCreateMethod(Class<?> queryClass, Class<?> browserClass) throws NoSuchMethodException {
        try {
            return queryClass.getMethod("create", browserClass);
        } catch (NoSuchMethodException ignored) {
        }

        for (Method method : queryClass.getMethods()) {
            if (!"create".equals(method.getName()) || method.getParameterCount() != 1) {
                continue;
            }
            Class<?> parameter = method.getParameterTypes()[0];
            if (parameter.isAssignableFrom(browserClass)) {
                return method;
            }
        }
        throw new NoSuchMethodException("JBCefJSQuery.create(" + browserClass.getName() + ")");
    }
}
