string(REGEX REPLACE "a" "x\\éy" out "aaa")
message("[${out}]")
