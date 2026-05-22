; PHP: classes, interfaces, functions, methods
;
; Node names per tree-sitter-php 0.24 grammar:
;   - function_definition     (top-level functions)
;   - method_declaration      (class/interface methods)
;   - class_declaration       (classes)
;   - interface_declaration   (interfaces)
;
; Verified 2026-05-08 against tree-sitter-php 0.24.2 sexp output.
; Earlier query used `function_declaration` which is NOT a valid node
; name → Query::new returned QueryError {NodeType "function_declaration"}
; → list_symbols_treesitter returned None → semantic::tests::test_list_symbols_php
; panicked on unwrap.

(class_declaration
  name: (name) @name) @definition

(interface_declaration
  name: (name) @name) @definition

(function_definition
  name: (name) @name) @definition

(method_declaration
  name: (name) @name) @definition
