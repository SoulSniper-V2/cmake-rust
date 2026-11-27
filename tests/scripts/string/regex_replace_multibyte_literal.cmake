# A multi-byte literal around a back-reference stays intact.
string(REGEX REPLACE "(a)" "é\\1é\\n" out "bab")
message("[${out}]")
set(l ab cb)
list(TRANSFORM l REPLACE "(b)" "€\\1€")
message("[${l}]")
