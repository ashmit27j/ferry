// A constant-rate HTTP load generator for the kind chaos phase (e2e.sh):
// standard library only, so the runner image needs nothing else. Requests
// start on a fixed schedule whether or not earlier ones finished (no
// coordinated omission). Prints a JSON summary: counts by status, errors by
// kind, latency percentiles.
//
//	go run load.go -url http://ADDR/smoke/hello -host smoke.example -rate 500 -duration 180s
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"sort"
	"strings"
	"sync"
	"time"
)

func main() {
	url := flag.String("url", "", "target URL")
	host := flag.String("host", "", "Host header")
	rate := flag.Int("rate", 500, "requests per second")
	duration := flag.Duration("duration", 3*time.Minute, "how long to send")
	flag.Parse()

	client := &http.Client{
		Timeout: 10 * time.Second,
		Transport: &http.Transport{
			MaxIdleConnsPerHost: 256,
			IdleConnTimeout:     30 * time.Second,
		},
	}
	var (
		mu       sync.Mutex
		statuses = map[int]int{}
		errs     = map[string]int{}
		lat      []time.Duration
		wg       sync.WaitGroup
	)
	interval := time.Second / time.Duration(*rate)
	ctx, cancel := context.WithTimeout(context.Background(), *duration)
	defer cancel()
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	start := time.Now()
	sent := 0
loop:
	for {
		select {
		case <-ctx.Done():
			break loop
		case <-ticker.C:
		}
		sent++
		wg.Add(1)
		go func() {
			defer wg.Done()
			req, _ := http.NewRequest(http.MethodGet, *url, nil)
			if *host != "" {
				req.Host = *host
			}
			t0 := time.Now()
			resp, err := client.Do(req)
			d := time.Since(t0)
			mu.Lock()
			defer mu.Unlock()
			if err != nil {
				kind := err.Error()
				if i := strings.LastIndex(kind, ": "); i >= 0 {
					kind = kind[i+2:]
				}
				errs[kind]++
				return
			}
			io.Copy(io.Discard, resp.Body)
			resp.Body.Close()
			statuses[resp.StatusCode]++
			lat = append(lat, d)
		}()
	}
	wg.Wait()
	elapsed := time.Since(start)

	sort.Slice(lat, func(i, j int) bool { return lat[i] < lat[j] })
	pct := func(p float64) float64 {
		if len(lat) == 0 {
			return 0
		}
		return float64(lat[int(p*float64(len(lat)-1))].Microseconds()) / 1000
	}
	ok := statuses[200]
	failed := sent - ok
	summary := map[string]any{
		"sent":            sent,
		"ok":              ok,
		"failed":          failed,
		"success_ratio":   float64(ok) / float64(max(sent, 1)),
		"statuses":        statuses,
		"errors":          errs,
		"seconds":         elapsed.Seconds(),
		"p50_ms":          pct(0.50),
		"p99_ms":          pct(0.99),
		"max_ms":          pct(1.0),
		"rate_per_second": *rate,
	}
	out, _ := json.Marshal(summary)
	fmt.Println(string(out))
	if failed > 0 {
		os.Exit(1)
	}
}
