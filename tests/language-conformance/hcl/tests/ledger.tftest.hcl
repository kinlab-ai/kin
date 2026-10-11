run "total" {
  command = plan

  assert {
    condition     = output.total == 6
    error_message = "the total is the sum of the scaled values"
  }
}
