set(ENV{SOURCE_DATE_EPOCH} "invalid-integer")
string(TIMESTAMP a "%Y" UTC)
message("unreached")
