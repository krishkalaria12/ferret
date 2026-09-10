; perl highlights for Ferret’s preview pane.
; Hand-written for tree-sitter-perl 1.1.2 (MIT)
; https://github.com/ganezdragon/tree-sitter-perl
; The crate ships no queries and nvim-treesitter's target a different
; grammar generation, so this covers the core surface instead: comments,
; strings, numbers, keywords, subs and variables. Field names verified
; against the pinned grammar's node-types.json.
; 41-slot adaptations: none — written directly in the 41-slot vocabulary.

(comments) @comment @spell

(pod_statement) @comment

[
  (integer)
  (floating_point)
  (hexadecimal)
  (octal)
  (scientific_notation)
] @number

[
  (true)
  (false)
] @boolean

[
  (string_single_quoted)
  (string_double_quoted)
  (string_q_quoted)
  (string_qq_quoted)
  (backtick_quoted)
  (command_qx_quoted)
  (heredoc_body_statement)
] @string

(escape_sequence) @string.escape

(function_definition
  name: (identifier) @function)

(call_expression_with_bareword
  function_name: (_) @function.call)

(method_invocation
  function_name: (_) @function.method)

(variable_declaration
  variable_name: (_) @variable)

[
  (scalar_variable)
  (array_variable)
  (hash_variable)
  (special_scalar_variable)
] @variable

(package_statement
  (package_name) @type)

(module_name) @type

[
  "sub"
  "my"
  "our"
  "local"
  "state"
  "package"
  "use"
  "no"
  "require"
  "return"
  "if"
  "elsif"
  "else"
  "unless"
  "while"
  "until"
  "for"
  "foreach"
  "last"
  "next"
  "redo"
  "goto"
] @keyword
