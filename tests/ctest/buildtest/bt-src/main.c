#include <stdio.h>
int main(int argc, char** argv)
{
  int i;
  fputs("bt", stdout);
  for (i = 1; i < argc; ++i) {
    printf(" <%s>", argv[i]);
  }
  fputc('\n', stdout);
  return TEST_RESULT;
}
