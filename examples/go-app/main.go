package main

import (
	"fmt"
	"net/http"
	"os"
)

func main() {
	port := os.Getenv("PORT")
	if port == "" { port = "3000" }
	http.HandleFunc("/", func(w http.ResponseWriter, _ *http.Request) { fmt.Fprintln(w, "ciao go example") })
	_ = http.ListenAndServe("127.0.0.1:"+port, nil)
}
