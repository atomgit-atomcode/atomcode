; Kotlin: classes/interfaces, objects, functions, properties, enum entries.
; (interfaces and enum classes are `class_declaration` in the tree-sitter-kotlin
; grammar, so one pattern covers class + interface + enum class.)

(class_declaration
  (type_identifier) @name) @definition

; `companion object Name` parses cleanly. NOTE: a top-level named
; `object Foo { … }` is mis-parsed by tree-sitter-kotlin-sg as an
; `object_literal` infix expression, so its NAME can't be captured as a
; declaration here — but its members (functions/properties) still are, so code
; inside objects stays indexed. `object_declaration` is kept for the contexts
; that do produce it.
(object_declaration
  (type_identifier) @name) @definition

(companion_object
  (type_identifier) @name) @definition

(function_declaration
  (simple_identifier) @name) @definition

(property_declaration
  (variable_declaration
    (simple_identifier) @name)) @definition

(enum_entry
  (simple_identifier) @name) @definition
