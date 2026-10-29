Re-record the demo video (server must be running on :7878):

    cd /tmp/vibery-demo && npm i playwright && node /path/to/web/demo/demo.mjs
    ffmpeg -i out/*.webm -c:v libx264 -crf 22 -pix_fmt yuv420p -movflags +faststart ../web/notes/vibery-demo.mp4
