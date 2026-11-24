# Regex matching is byte-wise.  Groups that split a multi-byte character
# must not crash; whole-character results round-trip exactly.
string(REGEX MATCHALL "." m "é€")
list(LENGTH m n)
string(REGEX MATCH "(.)(.)" m2 "é")
message("matchall=${n} count=${CMAKE_MATCH_COUNT}")
string(REGEX REPLACE "(.)" "\\1" r "aé€b")
string(REGEX MATCHALL "é" m3 "aébé")
set(L "é;xé")
list(TRANSFORM L REPLACE "(.)" "\\1" OUTPUT_VARIABLE t)
list(TRANSFORM L REPLACE "(.)(.)" "\\2\\1" OUTPUT_VARIABLE t2)
list(LENGTH t2 n2)
message("${r}|${m3}|${t}|${n2}")
