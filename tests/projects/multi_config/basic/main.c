int stat_value(void); int shared_value(void); int main(void) { return stat_value() + shared_value() == 5 ? 0 : 1; }
