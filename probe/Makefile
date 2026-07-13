all: forge_probe.dylib test_train

forge_probe.dylib: forge_probe.mm
	clang++ -dynamiclib -fobjc-arc -O2 -framework Metal -framework Foundation -o $@ $<

test_train: test_train.mm
	clang++ -fobjc-arc -O2 -framework Metal -framework Foundation -o $@ $<

clean:
	rm -f forge_probe.dylib test_train probe.log
