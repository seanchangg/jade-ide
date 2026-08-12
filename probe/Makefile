all: jade_probe.dylib test_train

jade_probe.dylib: jade_probe.mm
	clang++ -dynamiclib -fobjc-arc -O2 -framework Metal -framework Foundation -o $@ $<

test_train: test_train.mm
	clang++ -fobjc-arc -O2 -framework Metal -framework Foundation -o $@ $<

clean:
	rm -f jade_probe.dylib test_train probe.log
