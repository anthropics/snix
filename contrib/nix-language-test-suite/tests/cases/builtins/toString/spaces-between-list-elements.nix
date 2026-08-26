# The rule is: a space is emitted after every non-final element
# except when that element is an empty nested list.
[
  # a single space is emitted
  (toString ["single_space->" []])

  # Two non-empty list - two spaces
  (toString ["two->" "spaces->" []])

  # the first list's element is a non-empty list, so the space is emitted
  (toString [["non_empty_list-thus-one-space->"] ""])

  # no spaces emitted because the first element is an empty list
  (toString [[] "<-starts_with_empty_list-thus-no-space"])

  # the rule is recursive, so
  #
  # two spaces here:
  #  - first element is `[["space->" []]` -> a non-empty list -> emit a space
  #    - `"space->"` is the first nested element -> not an empty list -> emit a space
  (toString [["space->" []] "<-space"])

  # one space here:
  #  - first element is `[[] "<-no_space"]` -> a non-empty list -> emit a space
  #    - `[]` is the first nested element -> an empty list -> no space
  (toString [[[] "<-no_space"] "<-space"])
]
