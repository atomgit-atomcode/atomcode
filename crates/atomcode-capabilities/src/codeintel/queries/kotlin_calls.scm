; Kotlin call edges (@callee). Constructors look like plain calls in Kotlin
; (`Thing()`), so this also catches instantiations.

; direct call / constructor:  foo()  /  Thing()
(call_expression
  (simple_identifier) @callee
  (call_suffix))

; member / extension call:  obj.bar()  /  this.bar()
(call_expression
  (navigation_expression
    (navigation_suffix
      (simple_identifier) @callee))
  (call_suffix))
