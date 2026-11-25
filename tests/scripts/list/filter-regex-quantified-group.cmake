# list(FILTER ... REGEX) compiles with the same kwsys engine as string(REGEX):
# a quantified group whose branch has width is legal.
set(l a ab abb x éa)
list(FILTER l INCLUDE REGEX "(ab*)+")
message("include=[${l}]")
set(l a ab x éa)
list(FILTER l EXCLUDE REGEX "(éa*)+")
message("exclude=[${l}]")
set(l a)
list(FILTER l INCLUDE REGEX "(a*)+")
message("not reached")
