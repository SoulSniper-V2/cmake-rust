set(ENV{SOURCE_DATE_EPOCH} "123 trailing")
string(TIMESTAMP a "%Y" UTC)
message("unreached")
