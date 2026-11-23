extern "C" int gen(void);
int shared_fn();
int main(){return gen()+shared_fn()-1;}
