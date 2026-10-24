(binary_operator
  lhs: [(identifier) (string)] @name
  operator: ["<-" "<<-" "="]
  rhs: (function_definition)) @item

; RStudio code sections: a comment line ending in four or more `-`, `=` or `#`.
((comment) @name @item
  (#match? @name "^#.*[-=#]{4,}\\s*$"))
