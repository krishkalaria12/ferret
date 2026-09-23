; ini highlights for Ferret’s preview pane.
; Vendored from tree-sitter-ini 1.4.0 (Apache-2.0), queries/highlights.scm
; https://github.com/justinmk/tree-sitter-ini
; Adaptations: none — every capture resolves to the 41-slot palette.
(section_name
  (text) @type) ; consistency with toml

(comment) @comment @spell

[
  "["
  "]"
] @punctuation.bracket

"=" @operator

(setting
  (setting_name) @property)

; (setting_value) @none ; grammar does not support subtypes
