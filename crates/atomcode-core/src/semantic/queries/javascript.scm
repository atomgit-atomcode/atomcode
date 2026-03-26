; JavaScript/JSX/TSX: functions, classes, methods, arrow functions

(function_declaration
  name: (identifier) @name) @definition

(class_declaration
  name: (identifier) @name) @definition

(method_definition
  name: (property_identifier) @name) @definition

(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: (arrow_function))) @definition

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: (arrow_function))) @definition

(export_statement
  declaration: (function_declaration
    name: (identifier) @name)) @definition

(export_statement
  declaration: (class_declaration
    name: (identifier) @name)) @definition
