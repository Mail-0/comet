test("Keiki setup navigates sign-in, local appearance, and organizations", function()
	local app = mount("onboarding_order.luau")
	expect(app:text("#result")).to_be("ok")
end)
