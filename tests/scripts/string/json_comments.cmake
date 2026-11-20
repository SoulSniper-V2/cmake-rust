string(JSON a GET "{/*c*/\"a\":1,//trail
\"b\":2}" b)
message("a=${a}")
string(JSON b GET "// first comment
42")
message("b=${b}")
string(JSON c GET "/*only*/  [1]")
message("c=<${c}>")
