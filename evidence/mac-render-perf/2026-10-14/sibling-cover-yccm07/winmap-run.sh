#!/bin/sh
# Launch the new GUI with the cover test config, map windows at 15 s and 30 s.
cd ~/.cache/ftcover || exit 2
/usr/bin/python3 -I cover_park.py "$1" "$2" > "/tmp/winmap-$2.out" 2>&1 &
sleep 4
P=$(pgrep -f "Contents/MacOS/frankenterm-gui" | head -1)
echo "gui $P"
sleep 11; echo "--- t~15s"; ./winmap "$P"
sleep 15; echo "--- t~30s"; ./winmap "$P"
sleep 25; echo "--- t~55s"; ./winmap "$P"
wait
cat "/tmp/winmap-$2.out"
