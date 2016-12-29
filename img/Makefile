TARGET=$(patsubst %.dot,%.png,$(wildcard *.dot))

all: $(TARGET)
.PHONY: all

clean:
	$(RM) $(TARGET)
.PHONY: clean

%.png: %.dot
	dot -Tpng $< > $@
