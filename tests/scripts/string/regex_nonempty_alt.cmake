# kwsys NONEMPTY_AT_OFFSET: after an empty match, the next search rejects
# matches ending at the offset, so backtracking tries non-empty alternatives
# at the same position (e.g. (|b) picks "b").  The find_at(text,start,not_bol)
# API cannot express this flag; this test documents the exact oracle output.
cmake_policy(SET CMP0186 NEW)
string(REGEX MATCHALL "(|b)" a "abc")
message("a=<${a}>")
string(REGEX REPLACE "(|b)" "x" b "abc")
message("b=<${b}>")
string(REGEX MATCHALL "(|xy)" c "axyb")
message("c=<${c}>")
