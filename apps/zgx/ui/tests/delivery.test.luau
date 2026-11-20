local delivery = require("../screens/chat/delivery")

test("local outbox rows preserve sends without mutating them", function()
	local input = { { held = true, id = "send-1", text = "hello" }, { id = "other", text = "not held" } }
	local rows = delivery.outbox_rows(input, true)
	expect(#rows).to_be(1)
	expect(rows[1].text).to_be("hello")
	expect(rows[1].removable).to_be(true)
	expect(delivery.outbox_id(rows[1].id)).to_be("send-1")
	expect(input[1].id).to_be("send-1")
end)

test("an empty local outbox has no queue rows", function()
	expect(delivery.outbox_rows(nil, false)).to_equal({})
	expect(delivery.has_outbox({})).to_be(false)
	expect(delivery.outbox_id("another-row")).to_be_nil()
end)

test("failed local sends remain visible and can be removed only when supported", function()
	local list = { { held = true, id = "failed", failure = "upload stopped" } }
	local rows = delivery.outbox_rows(list, false)
	expect(delivery.has_outbox(list)).to_be(true)
	expect(#rows).to_be(1)
	expect(rows[1].waiting ~= "").to_be(true)
	expect(rows[1].removable).to_be(false)
end)

test("expired waiting sends are excluded and a failure is retained", function()
	local pending, failed = delivery.waiting_of({
		{ sent = 10, expires = 99 }, { sent = 50, expires = 101 }, { failure = "stopped", message = "failed-send" },
	}, 100)
	expect(#pending).to_be(1)
	expect(pending[1].at).to_be(50)
	expect(failed.message).to_be("failed-send")
end)
