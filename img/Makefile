REPOROOT=$(shell git rev-parse --show-toplevel)
IMGDIR=$(REPOROOT)/target/doc/jaba/_img
TARGET=$(patsubst %.dot,$(IMGDIR)/%.png,$(wildcard *.dot))

all: $(TARGET)
.PHONY: all

clean:
	$(RM) $(TARGET)
.PHONY: clean

$(IMGDIR)/%.png: %.dot | $(IMGDIR)
	dot -Tpng $< > $@

$(IMGDIR):
	mkdir -p $@
