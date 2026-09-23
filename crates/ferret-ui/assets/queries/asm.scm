; asm highlights for Ferret's preview pane.
; Vendored from tree-sitter-asm 0.24.0 (MIT), queries/asm/highlights.scm
; https://github.com/RubixDev/tree-sitter-asm
; 41-slot adaptations: none yet.
; General
(label
  [(ident) (word)] @label)

(reg) @variable.builtin

(meta
  kind: (_) @function.builtin)

(instruction
  kind: (_) @function.builtin)

(const
  name: (word) @constant)

; Comments
[
  (line_comment)
  (block_comment)
] @comment @spell

; Literals
(int) @number

(float) @number.float

(string) @string

; Keywords
[
  "byte"
  "word"
  "dword"
  "qword"
  "ptr"
  "rel"
  "label"
  "const"
] @keyword

; Operators & Punctuation
[
  "+"
  "-"
  "*"
  "/"
  "%"
  "|"
  "^"
  "&"
] @operator

[
  "("
  ")"
  "["
  "]"
] @punctuation.bracket

[
  ","
  ":"
] @punctuation.delimiter
