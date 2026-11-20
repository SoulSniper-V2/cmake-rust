file(MAKE_DIRECTORY b)
file(CREATE_LINK a b SYMBOLIC RESULT out)
message("[${out}]")
